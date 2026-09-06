//! Reads an authorization server's discovery document and says what it needs.
//!
//!     cargo run -p arvo-oauth --example discover -- https://agent.robinhood.com/mcp/trading
//!
//! Discovery only — nothing is registered and no browser opens. Exists because
//! "sign-in did not work" has three quite different causes (the discovery
//! address is wrong, the server does not support what a public client needs,
//! or the browser half failed) and this separates the first two from the third
//! without involving a person.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resource = std::env::args()
        .nth(1)
        .ok_or("usage: discover <resource-url>")?;

    let address = arvo_oauth::metadata_url(&resource)?;
    println!("{resource}");
    println!("  metadata at {address}");

    let document: serde_json::Value = tokio::runtime::Runtime::new()?
        .block_on(async { reqwest::get(&address).await?.json().await })?;

    match arvo_oauth::ServerMetadata::parse(&document) {
        Err(reason) => println!("  unusable: {reason}"),
        Ok(metadata) => {
            println!("  authorize at {}", metadata.authorization_endpoint);
            println!("  token at     {}", metadata.token_endpoint);
            match metadata.registration_endpoint.as_deref() {
                Some(endpoint) => println!("  register at  {endpoint}"),
                // Without it a desktop app has no way to obtain a client id
                // except a developer portal and a value baked into the binary.
                None => println!("  no dynamic registration: a client id must be supplied"),
            }
            println!("  scopes: {:?}", metadata.scopes);
        }
    }
    Ok(())
}
