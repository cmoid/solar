use anyhow::Result;
use solar_client::{Client, SolarClient};

const SERVER_ADDR: &str = "http://127.0.0.1:3030";
const PUB_KEY: &str = "@HEqy940T6uB+T+d9Jaa58aNfRzLx9eRWqkZljBmnkmk=.ed25519";

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::new(SERVER_ADDR.to_owned())?;

    let subscriptions = client.subscriptions(PUB_KEY).await?;
    println!("{:#?}", subscriptions);
    // [
    //     "falconry",
    //     "solarfest",
    //     "compendiumofmadscience",
    //     "inktober2018",
    //     "butt-summaries",
    //     "squatchcam",
    //     ...
    //     "arduino",
    //     "savingspools",
    //     "meditation",
    // ]

    Ok(())
}
