# Deployments

`scripts/deploy.sh` writes one JSON file per network here, for example
`deployments/testnet.json`, with every contract ID and the admin account.
The backend reads these IDs from its environment (`STELLAR_*_CONTRACT_ID`).

Testnet files may be committed. Never commit identity secrets.
