use anyhow::{Context, Result};
#[tokio::main]
async fn main() -> Result<()> {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".data/store".into());
    let token = std::env::var("FLICKER_STORE_TOKEN")
        .context("Set FLICKER_STORE_TOKEN to a random secret of at least 24 characters")?;
    let store = flicker::network::start_store(dir, token).await?;
    println!("Flicker store • iroh 1.3.0");
    println!("Endpoint: {}", store.endpoint.id());
    println!("Encrypted objects expire within 24 hours. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    store.endpoint.close().await;
    Ok(())
}
