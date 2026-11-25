// Installed-package smoke test: the released crate builds as a dependency,
// and a call reaches its HTTP stack (a closed port: a transport error).
use qucelerate_ai_gateway::{ChatRequest, Client, Error, Message};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let client = Client::builder("http://127.0.0.1:1").api_key("gw_smoke").build().expect("client");
    let request = ChatRequest::new("smoke", vec![Message::user("hi")]);
    match client.chat(&request).await {
        Err(Error::Transport(_)) => println!("ok: rust client installs and runs"),
        other => {
            eprintln!("expected a transport error, got {other:?}");
            std::process::exit(1);
        }
    }
}
