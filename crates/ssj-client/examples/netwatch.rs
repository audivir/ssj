//! Prints every notification of the network watch. Run it, then switch Wi-Fi off and on, change
//! the network, or put the machine to sleep and wake it. Stop it with Ctrl-C.

#[tokio::main]
async fn main() {
    let mut sources = ssj_client::netwatch::start();
    println!("sources: {:?}", sources.active);
    let started = std::time::Instant::now();
    while let Some(cause) = ssj_client::netwatch::next_settled(
        &mut sources.events,
        std::time::Duration::from_millis(300),
    )
    .await
    {
        println!("{:>7.1}s  {cause:?}", started.elapsed().as_secs_f32());
    }
}
