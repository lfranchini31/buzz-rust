use std::sync::Arc;

use crate::datasource::{CatalogTable, HCombTable};
use crate::error::Result;
use crate::models::query::{BuzzStep, BuzzStepType};
use crate::not_impl_err;
use datafusion::execution::context::ExecutionContext;
use datafusion::logical_plan::LogicalPlan;
use futures::future::{BoxFuture, FutureExt};

pub struct QueryPlanner {
    /// This execution context is not meant to run queries but only to plan them.
    execution_context: ExecutionContext,
}

pub struct ZonePlan {
    pub hbee: Vec<LogicalPlan>,
    pub hcomb: LogicalPlan,
}

/// The plans to be distributed among hbees and hcombs
/// To transfer them over the wire, these logical plans should be serializable
pub struct DistributedPlan {
    /// One hcomb/hbee combination of plan for each zone.
    pub zones: Vec<ZonePlan>,
}

impl QueryPlanner {
    pub fn new() -> Self {
        Self {
            execution_context: ExecutionContext::new(),
        }
    }

    pub fn add_catalog(&mut self, name: &str, table: CatalogTable) {
        self.execution_context.register_table(name, Box::new(table));
    }

    pub async fn plan(
        &mut self,
        query_id: String,
        query_steps: Vec<BuzzStep>,
        nb_hcomb: i16,
    ) -> Result<DistributedPlan> {
        // TODO lift the limitation inforced by the following assert:
        assert!(
            query_steps.len() == 2
                && query_steps[0].step_type == BuzzStepType::HBee
                && query_steps[1].step_type == BuzzStepType::HComb,
            "You must have one exactly one HBee step followed by one HComb step for now"
        );

        let bee_df = self.execution_context.sql(&query_steps[0].sql)?;
        let src_bee_plan = bee_df.to_logical_plan();
        let bee_output_schema = src_bee_plan.schema().as_ref().clone();
        let bee_plans = self.split(&src_bee_plan).await?;

        // register a handle to the intermediate table on the context
        let result_table =
            HCombTable::new(query_id, bee_plans.len(), bee_output_schema.into());
        self.execution_context
            .register_table(&query_steps[0].name, Box::new(result_table));

        // run the hcomb part of the query
        let hcomb_df = self.execution_context.sql(&query_steps[1].sql)?;
        let hcomb_plan = hcomb_df.to_logical_plan();

        // TODO check that the source is a valid hcomb provider

        // If they are less hbees than hcombs, don't use all hcombs
        let used_hcomb = std::cmp::min(nb_hcomb as usize, bee_plans.len());

        // init plans for each zone
        let mut zones = (0..used_hcomb)
            .map(|_i| ZonePlan {
                hbee: vec![],
                hcomb: hcomb_plan.clone(),
            })
            .collect::<Vec<_>>();
        // distribute hbee plans between zones
        bee_plans
            .into_iter()
            .enumerate()
            .for_each(|(i, bee_plan)| zones[i % used_hcomb].hbee.push(bee_plan));

        Ok(DistributedPlan { zones: zones })
    }

    /// Takes a plan and if the source is a catalog, it distibutes the files accordingly
    /// Each resulting logical plan is a good workload for a given bee
    /// Only works with linear plans (only one datasource)
    /// TODO could this be implem as an optim rule?
    fn split<'a>(
        &'a mut self,
        plan: &'a LogicalPlan,
    ) -> BoxFuture<'a, Result<Vec<LogicalPlan>>> {
        async move {
            let new_inputs = datafusion::optimizer::utils::inputs(&plan);
            if new_inputs.len() > 1 {
                Err(not_impl_err!(
                    "Operations with more than one inputs are not supported",
                ))
            } else if new_inputs.len() == 1 {
                let exprs = datafusion::optimizer::utils::expressions(&plan);
                let inputs = self.split(new_inputs[0]).await?;
                inputs
                    .into_iter()
                    .map(|lp| -> Result<LogicalPlan> {
                        Ok(datafusion::optimizer::utils::from_plan(
                            plan,
                            &exprs,
                            &vec![lp],
                        )?)
                    })
                    .collect::<Result<Vec<_>>>()
            } else if let Some(catalog_table) = Self::as_catalog(&plan) {
                catalog_table
                    .split()
                    .into_iter()
                    .map(|item| {
                        Ok(self
                            .execution_context
                            .read_table(Arc::new(item))?
                            .to_logical_plan())
                    })
                    .collect()
            } else {
                Ok(vec![plan.clone()])
            }
        }
        .boxed() // recursion in an `async fn` requires boxing
    }

    fn as_catalog<'a>(plan: &'a LogicalPlan) -> Option<&'a CatalogTable> {
        if let LogicalPlan::TableScan { source: table, .. } = plan {
            table.as_any().downcast_ref::<CatalogTable>()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::{CatalogTable, HBeeTable, S3ParquetTable, SplittableTable};
    use crate::models::SizedFile;
    use arrow::datatypes::{Schema, SchemaRef};
    use datafusion::datasource::datasource::Statistics;

    #[tokio::test]
    async fn test_simple_query() {
        let mut planner = QueryPlanner::new();
        let nb_split = 5;
        planner.add_catalog(
            "test",
            CatalogTable::new(Box::new(MockSplittableTable(nb_split))),
        );

        let steps = vec![
            BuzzStep {
                sql: "SELECT * FROM test".to_owned(),
                name: "mapper".to_owned(),
                step_type: BuzzStepType::HBee,
            },
            BuzzStep {
                sql: "SELECT * FROM mapper".to_owned(),
                name: "reducer".to_owned(),
                step_type: BuzzStepType::HComb,
            },
        ];

        let plan_res = planner.plan("mock_query_id".to_owned(), steps, 1).await;
        assert!(plan_res.is_ok(), "The planner failed on a simple query");
        let plan = plan_res.unwrap();
        assert_eq!(plan.zones.len(), 1);
        assert_eq!(plan.zones[0].hbee.len(), 5);
    }

    #[tokio::test]
    async fn test_query_unknown_table() {
        let mut planner = QueryPlanner::new();
        let steps = vec![
            BuzzStep {
                sql: "SELECT * FROM test".to_owned(),
                name: "mapper".to_owned(),
                step_type: BuzzStepType::HBee,
            },
            BuzzStep {
                sql: "SELECT * FROM mapper".to_owned(),
                name: "reducer".to_owned(),
                step_type: BuzzStepType::HComb,
            },
        ];

        let plan_res = planner.plan("mock_query_id".to_owned(), steps, 1).await;
        assert!(
            plan_res.is_err(),
            "The planner should have failed as the 'test' table is not defined"
        );
    }

    //// Test Fixtures: ////

    /// A SplittableTable that splits into (usize) S3Parquet tables
    struct MockSplittableTable(usize);

    impl SplittableTable for MockSplittableTable {
        fn split(&self) -> Vec<HBeeTable> {
            (0..self.0)
                .map(|i| {
                    S3ParquetTable::new(
                        "north-pole-1".to_owned(),
                        "santas-bucket".to_owned(),
                        vec![SizedFile {
                            key: format!("gift_{}", i),
                            length: 999999999,
                        }],
                        Arc::new(Schema::empty()),
                    )
                })
                .collect::<Vec<_>>()
        }
        fn schema(&self) -> SchemaRef {
            Arc::new(Schema::empty())
        }
        fn statistics(&self) -> Statistics {
            Statistics::default()
        }
    }
}
