use anyhow::Context;
use wtransport::Identity;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cert_path = std::env::var_os("WT_CERT_PATH").unwrap_or_else(|| "fullchain.pem".into());
    let key_path = std::env::var_os("WT_KEY_PATH").unwrap_or_else(|| "privkey.pem".into());
    let identity = Identity::self_signed(["localhost", "127.0.0.1"])?;
    identity
        .certificate_chain()
        .store_pemfile(&cert_path)
        .await
        .with_context(|| format!("write {}", cert_path.to_string_lossy()))?;
    identity
        .private_key()
        .store_secret_pemfile(&key_path)
        .await
        .with_context(|| format!("write {}", key_path.to_string_lossy()))?;
    println!("certificate: {}", cert_path.to_string_lossy());
    println!("private key: {}", key_path.to_string_lossy());
    Ok(())
}
