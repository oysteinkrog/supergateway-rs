mod child;
mod cli;
mod client;
mod codec;
mod cors;
mod error;
mod gateway;
mod health;
mod jsonrpc;
mod observe;
mod session;
mod signal;

use cli::{InputMode, OutputTransport};

fn main() -> anyhow::Result<()> {
    let rt = asupersync::runtime::RuntimeBuilder::new().build()?;
    let rt_handle = rt.handle();
    rt.block_on(async_main(rt_handle))
}

async fn async_main(rt_handle: asupersync::runtime::RuntimeHandle) -> anyhow::Result<()> {
    let config = cli::Config::parse();

    match (&config.input_mode, &config.output_transport) {
        (InputMode::Stdio, OutputTransport::Sse) => gateway::sse::run(config).await,
        (InputMode::Stdio, OutputTransport::Ws) => gateway::ws::run(config).await,
        (InputMode::Stdio, OutputTransport::StreamableHttp) if config.stateful => {
            gateway::stateful_http::run(config, rt_handle).await
        }
        (InputMode::Stdio, OutputTransport::StreamableHttp) => {
            gateway::stateless_http::run(config).await
        }
        (InputMode::Sse, OutputTransport::Stdio) => gateway::sse_to_stdio::run(config).await,
        (InputMode::StreamableHttp, OutputTransport::Stdio) => {
            gateway::http_to_stdio::run(config).await
        }
        _ => anyhow::bail!(
            "unsupported transport combination: {:?} -> {}",
            config.input_mode,
            config.output_transport
        ),
    }
}
