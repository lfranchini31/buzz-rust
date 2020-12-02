use fmt::Debug;
use std::any::Any;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{fmt, thread};

use crate::s3::S3FileAsync;
use arrow::datatypes::{Schema, SchemaRef};
use arrow::error::{ArrowError, Result as ArrowResult};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_plan::Partitioning;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::stream::Stream;
use parquet::arrow::{ArrowReader, ParquetFileArrowReader};
use parquet::file::reader::{FileReader, Length, SerializedFileReader};

/// Execution plan for scanning a Parquet file
#[derive(Debug, Clone)]
pub struct ParquetExec {
    files: Vec<S3FileAsync>,
    /// Schema after projection is applied
    schema: SchemaRef,
    /// Projection for which columns to load
    projection: Vec<usize>,
    /// Batch size
    batch_size: usize,
}

fn path_to_reader(file: S3FileAsync) -> ParquetFileArrowReader {
    let file_reader = Arc::new(
        SerializedFileReader::new(file).expect("Failed to create serialized reader"),
    );
    ParquetFileArrowReader::new(file_reader)
}

impl ParquetExec {
    /// Create a new Parquet reader execution plan
    pub fn try_new(
        files: Vec<S3FileAsync>,
        projection: Option<Vec<usize>>,
        batch_size: usize,
        schema: SchemaRef,
    ) -> Result<Self> {
        let projection = match projection {
            Some(p) => p,
            None => (0..schema.fields().len()).collect(),
        };

        for i in 0..files.len() {
            Self::download_footer(files[i].clone());
            let file_reader = Arc::new(
                SerializedFileReader::new(files[i].clone())
                    .expect("Failed to create serialized reader"),
            );
            let mut arrow_reader = ParquetFileArrowReader::new(file_reader.clone());
            // TODO what about metadata ?
            if schema.fields() != arrow_reader.get_schema()?.fields() {
                return Err(DataFusionError::Plan(
                    "Expected and parsed schema fields are not equal".to_owned(),
                ));
            }
            // prefetch usefull byte ranges
            let metadata = file_reader.metadata();
            for i in 0..metadata.num_row_groups() {
                for proj in &projection {
                    let rg_metadata = metadata.row_group(i);
                    let col_metadata = rg_metadata.column(*proj);
                    let (start, length) = col_metadata.byte_range();
                    files[i].prefetch(start, length as usize);
                }
            }
        }

        let projected_schema = Schema::new(
            projection
                .iter()
                .map(|col| schema.field(*col).clone())
                .collect(),
        );

        Ok(Self {
            files,
            schema: Arc::new(projected_schema),
            projection,
            batch_size,
        })
    }

    fn download_footer(file: S3FileAsync) {
        let end_length = 1024 * 1024;
        let (end_start, end_length) = match file.len().checked_sub(end_length) {
            Some(val) => (val, end_length),
            None => (0, file.len()),
        };
        file.prefetch(end_start, end_length as usize);
    }
}

#[async_trait]
impl ExecutionPlan for ParquetExec {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        // this is a leaf node and has no children
        vec![]
    }

    /// Get the output partitioning of this plan
    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.files.len())
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.is_empty() {
            Ok(Arc::new(self.clone()))
        } else {
            Err(DataFusionError::Internal(format!(
                "Children cannot be replaced in {:?}",
                self
            )))
        }
    }

    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        // because the parquet implementation is not thread-safe, it is necessary to execute
        // on a thread and communicate with channels
        let (response_tx, response_rx): (
            SyncSender<Option<ArrowResult<RecordBatch>>>,
            Receiver<Option<ArrowResult<RecordBatch>>>,
        ) = sync_channel(2);

        let file = self.files[partition].clone();
        let projection = self.projection.clone();
        let batch_size = self.batch_size;

        thread::spawn(move || {
            if let Err(e) = read_file(file, projection, batch_size, response_tx) {
                println!("Parquet reader thread terminated due to error: {:?}", e);
            }
        });

        Ok(Box::pin(ParquetStream {
            schema: self.schema.clone(),
            response_rx,
        }))
    }
}

fn send_result(
    response_tx: &SyncSender<Option<ArrowResult<RecordBatch>>>,
    result: Option<ArrowResult<RecordBatch>>,
) -> Result<()> {
    response_tx
        .send(result)
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    Ok(())
}

fn read_file(
    file: S3FileAsync,
    projection: Vec<usize>,
    batch_size: usize,
    response_tx: SyncSender<Option<ArrowResult<RecordBatch>>>,
) -> Result<()> {
    let mut arrow_reader = path_to_reader(file.clone());
    let mut batch_reader =
        arrow_reader.get_record_reader_by_columns(projection.clone(), batch_size)?;
    loop {
        match batch_reader.next() {
            Some(Ok(batch)) => send_result(&response_tx, Some(Ok(batch)))?,
            None => {
                // finished reading file
                send_result(&response_tx, None)?;
                break;
            }
            Some(Err(e)) => {
                let err_msg =
                    format!("Error reading batch from {:?}: {}", file, e.to_string());
                // send error to operator
                send_result(
                    &response_tx,
                    Some(Err(ArrowError::ParquetError(err_msg.clone()))),
                )?;
                // terminate thread with error
                return Err(DataFusionError::Execution(err_msg));
            }
        }
    }
    Ok(())
}

struct ParquetStream {
    schema: SchemaRef,
    response_rx: Receiver<Option<ArrowResult<RecordBatch>>>,
}

impl Stream for ParquetStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.response_rx.recv() {
            Ok(batch) => Poll::Ready(batch),
            // RecvError means receiver has exited and closed the channel
            Err(_) => Poll::Ready(None),
        }
    }
}

impl RecordBatchStream for ParquetStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test() -> Result<()> {
        Ok(())
    }
}
