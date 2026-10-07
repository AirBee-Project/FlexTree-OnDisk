use FlexTree_OnDisk::table::Table;
use kasane_logic::SingleId;

#[compio::main]
async fn main() {
    let mut table = Table::open("test.kasane").await.unwrap();
    table
        .insert(SingleId::new(13, 4, 4, 4).unwrap(), 1)
        .await
        .unwrap()
}
