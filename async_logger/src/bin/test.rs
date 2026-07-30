use async_logger::Builder;
use log;
use tokio;

#[tokio::main]
async fn main() {
    let async_logger_holder = Builder::new()
        .bound(8192)
        .max_level(log::LevelFilter::Debug)
        .stdout(true)
        //.rotated_file("test.log", 5 * 1024, 3)
        .setup();

    log::info!("Hello, async_logger");
    log::info!("Bye");

    async_logger_holder.shutdown().await;
}
