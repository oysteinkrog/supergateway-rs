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
mod serve;
mod session;
mod signal;

use cli::{InputMode, OutputTransport};

fn main() -> anyhow::Result<()> {
    let rt = asupersync::runtime::RuntimeBuilder::new().build()?;
    let rt_handle = rt.handle();
    // Spawn as a proper task so async_main runs with a Cx context.
    let join = rt_handle.spawn(async_main(rt_handle.clone()));
    rt.block_on(join)
}

async fn async_main(rt_handle: asupersync::runtime::RuntimeHandle) -> anyhow::Result<()> {
    let cx = asupersync::Cx::current().expect("async_main must run in task context");
    let config = cli::Config::parse();

    match (&config.input_mode, &config.output_transport) {
        (InputMode::Stdio, OutputTransport::Sse) => gateway::sse::run(&cx, config, rt_handle).await,
        (InputMode::Stdio, OutputTransport::Ws) => gateway::ws::run(&cx, config, rt_handle).await,
        (InputMode::Stdio, OutputTransport::StreamableHttp) if config.stateful => {
            gateway::stateful_http::run(&cx, config, rt_handle).await
        }
        (InputMode::Stdio, OutputTransport::StreamableHttp) => {
            gateway::stateless_http::run(&cx, config, rt_handle).await
        }
        (InputMode::Sse, OutputTransport::Stdio) => gateway::sse_to_stdio::run(&cx, config).await,
        (InputMode::StreamableHttp, OutputTransport::Stdio) => {
            gateway::http_to_stdio::run(&cx, config).await
        }
        _ => anyhow::bail!(
            "unsupported transport combination: {:?} -> {}",
            config.input_mode,
            config.output_transport
        ),
    }
}
