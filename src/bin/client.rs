use clap::Parser;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    my_frp::common::init();
    my_frp::client::run(my_frp::client::Options::parse()).await
}
