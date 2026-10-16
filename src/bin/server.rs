#[tokio::main]
async fn main() -> anyhow::Result<()> {
    my_frp::common::init();
    let mut args = std::env::args_os().skip(1);
    let path = args.next().unwrap_or_else(|| "server.json".into());
    if args.next().is_some() || path.to_string_lossy().starts_with('-') {
        anyhow::bail!(
            "用法: myfrps [配置文件.json]；默认读取当前目录的 server.json，服务端参数请在 JSON 中设置"
        );
    }
    my_frp::server::run(my_frp::server::Config::load(path)?).await
}
