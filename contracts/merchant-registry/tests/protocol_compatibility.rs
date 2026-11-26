use merchant_registry::{MerchantRegistry, MerchantRegistryClient};
use soroban_sdk::{testutils::{Address as _, Ledger}, vec, Address, Env, String};

#[test]
fn merchant_lifecycle_works_on_selected_protocol() {
    let protocol: u32 = std::env::var("STELLARPOS_PROTOCOL_VERSION")
        .unwrap_or_else(|_| "27".to_owned())
        .parse()
        .expect("protocol version must be an integer");

    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_protocol_version(protocol);

    let contract_id = env.register(MerchantRegistry, ());
    let client = MerchantRegistryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let owner = Address::generate(&env);
    let settlement = Address::generate(&env);
    let asset = Address::generate(&env);

    client.initialize(&admin);
    client.register(
        &owner,
        &String::from_str(&env, "Protocol Matrix Merchant"),
        &settlement,
        &vec![&env, asset.clone()],
    );
    client.approve(&owner);

    let merchant = client.get_merchant(&owner);
    assert_eq!(merchant.settlement, settlement);
    assert!(client.accepts(&owner, &asset));
}
