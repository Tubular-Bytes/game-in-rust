use uuid::Uuid;

use building_game::actor::model::Task;

#[tokio::main]
async fn main() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .pretty()
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");

    tracing::info!("Starting the application...");

    let mut dispatcher = building_game::actor::dispatcher::Dispatcher::new();

    dispatcher.listen(2).await;

    dispatcher.dispatch(Task { id: Uuid::new_v4() }).await;

    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");

    dispatcher.stop().await;

    tracing::info!("All workers have been stopped.");
}
