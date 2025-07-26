use building_game::{blueprint::model::Value, persistence::{inmemory::MemoryDatabase, worker::{Op, OpType, PersistenceWorker, Persister}}};
use uuid::Uuid;

#[tokio::main]
async fn main() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");
    tracing::info!("Starting the application...");

    let db = MemoryDatabase::new();

    let broker = building_game::actor::broker::Broker::new();
    let (store_tx, store_rx) = tokio::sync::mpsc::channel(100);
    
    // Create example inventory data and store it in the database
    let existing_inventory = example_inventory_data(&store_tx);
    db.set(
        format!("inventory:{}", existing_inventory.id.clone()),
        existing_inventory.serialize().unwrap(),
    ).unwrap();

    let mut persistence = PersistenceWorker::new(
        Box::new(db),
        store_rx,
    );

    let persistence_handle = tokio::spawn(async move {
        persistence.run().await;
    });

    // let inventory_id = Uuid::new_v4();
    // let _inventory = building_game::actor::inventory::Inventory::new(
    //     inventory_id,
    //     broker.clone().topic("inventory").sender.clone(),
    //     &store_tx,
    // );

    let _inventory = building_game::actor::inventory::Inventory::new(
        existing_inventory.id.clone(),
        broker.clone().topic("inventory").sender.clone(),
        &store_tx.clone(),
    );

    // Give enough time for the inventory to restore its data
    tracing::info!("Waiting for inventory restoration to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    tracing::info!("Proceeding to shutdown...");

    store_tx.send(Op{
        op_type: OpType::Stop,
        reply: None,
    }).await.unwrap();

    persistence_handle.await.unwrap();
}

fn example_inventory_data(persistence_tx: &tokio::sync::mpsc::Sender<building_game::persistence::worker::Op>) -> building_game::actor::inventory::Inventory {
    let id = Uuid::new_v4();
    let broker = building_game::actor::broker::Broker::new();
    
    let inv = building_game::actor::inventory::Inventory::new(id, broker.topic("inventory").sender.clone(), persistence_tx);

    inv.resources.lock().unwrap().insert("wood".to_string(), Value{
        name: "wood".to_string(),
        value: 100,
    });

    return inv;
}