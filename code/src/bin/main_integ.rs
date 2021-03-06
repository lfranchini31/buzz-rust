mod main_fuse_local;
mod main_hbee_local;
mod main_hcomb;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::select! {
        res = main_fuse_local::start_fuse("localhost", "localhost") => {
            println!("[integ] fuse result: {:?}", res);
        }
        res = main_hbee_local::start_hbee_server() => {
            println!("[integ] hbee server failed: {:?}", res);
        }
        res = main_hcomb::start_hcomb_server() => {
            println!("[integ] hcomb server failed: {:?}", res);
        }
    }
    Ok(())
}
