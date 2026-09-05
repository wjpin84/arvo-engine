//! Points the client at a real MCP endpoint and reports what came back.
//!
//!     cargo run -p arvo-mcp --example probe -- <endpoint> [token]
//!
//! With no token this is still worth running: a real server answering with a
//! clean `Unauthorized` exercises the entire HTTP path — URL, headers, status
//! handling, error mapping — against something other than a fixture. The unit
//! tests cover the framing; this covers everything the framing sits on.

use arvo_mcp::{ClientError, McpClient};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(endpoint) = args.next() else {
        eprintln!("usage: probe <endpoint> [token]");
        std::process::exit(2);
    };
    let token = args.next().unwrap_or_default();

    println!("endpoint: {endpoint}");
    println!(
        "token:    {}",
        if token.is_empty() {
            "(none — expecting the server to refuse)"
        } else {
            "(supplied)"
        }
    );

    let client = McpClient::new(&endpoint, token);
    match client.connect().await {
        Ok(version) => println!("\nconnected, server speaks {version}"),
        Err(ClientError::Unauthorized { status }) => {
            println!("\nrefused with HTTP {status} — the transport reached a real MCP server");
            println!("and the token path is what remains. This is the expected result");
            println!("without credentials.");
            return;
        }
        Err(err) => {
            println!("\nfailed: {err}");
            // The chain, not just the outer message: the cause is usually
            // where the actual reason lives.
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                println!("  caused by: {cause}");
                source = cause.source();
            }
            return;
        }
    }

    match client.list_tools().await {
        Ok(tools) => {
            println!("\n{} tools offered:", tools.len());
            for tool in tools.iter().take(40) {
                println!("  {}", tool.name);
            }
            if tools.len() > 40 {
                println!("  … and {} more", tools.len() - 40);
            }
        }
        Err(err) => println!("\nlisting tools failed: {err}"),
    }
}
