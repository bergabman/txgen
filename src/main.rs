use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::stream::{FuturesUnordered, StreamExt};
use rand::{
    Rng,
    seq::{IndexedRandom, SliceRandom},
};
use serde_json::json;
use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient,
    rpc_config::RpcRequestAirdropConfig,
};
use solana_rpc_client_api::request::RpcRequest;
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    hash::Hash,
    native_token::LAMPORTS_PER_SOL,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use spl_token_2022::{
    instruction::{initialize_mint, mint_to},
    state::Mint,
};
use tokio::time::Instant;
use tracing::{error, info};

#[derive(Clone)]
struct LatestHash(Hash, Instant);

impl LatestHash {
    async fn refresh(&mut self, rpc_client: &RpcClient) -> Result<()> {
        if self.1.elapsed().as_secs() > 5 {
            self.0 = rpc_client.get_latest_blockhash().await?;
            self.1 = Instant::now();
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let rpc_url = args.get(1).expect("rpc url must be an argument");
    tracing_subscriber::fmt::init();
    info!("Generating keypairs");
    // let rpc_url = "https://api.devnet.solana.com";
    // let rpc_url = "http://127.0.0.1:8899";
    let rpc_client = RpcClient::new(rpc_url.into());

    // Generate keypairs for transactions
    let keypairs = (0..50).map(|_| Keypair::new()).collect::<Vec<Keypair>>();

    if let Err(err) = request_airdrops(&keypairs, &rpc_client).await {
        error!("Airdrops failed {}", err.to_string())
    }
    let mut mint = Pubkey::new_unique();

    let mint = loop {
        tokio::time::sleep(Duration::from_secs(4)).await;
        match setup_token(&keypairs, &rpc_client).await {
            Ok(mint) => break mint,
            Err(err) => error!("{}", err.to_string()),
        }
    };
    // let mint = setup_token(&keypairs, &rpc_client).await.unwrap();

    if let Err(err) = start_sending_txs(&keypairs, &rpc_client, mint).await {
        error!("Airdrops failed {}", err.to_string())
    }

    Ok(())
}

async fn request_airdrops(keypairs: &Vec<Keypair>, rpc_client: &RpcClient) -> Result<()> {
    let config = RpcRequestAirdropConfig {
        recent_blockhash: Some(rpc_client.get_latest_blockhash().await?.to_string()),
        commitment: Some(CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        }),
    };

    // Collect futures into FuturesUnordered
    let mut ftrs: FuturesUnordered<_> = keypairs
        .iter()
        .map(|kp| {
            rpc_client.send::<String>(
                RpcRequest::RequestAirdrop,
                json!([
                    kp.pubkey().to_string(),
                    5 * LAMPORTS_PER_SOL,
                    config.clone()
                ]),
            )
        })
        .collect();

    // Process the airdrop futures
    while let Some(result) = ftrs.next().await {
        if let Err(why) = result {
            error!("airdrop tx failed: {}", why)
        }
        // println!("airdrop tx https://explorer.solana.com/tx/{}?cluster=custom", result?)
    }

    Ok(())
}

async fn start_sending_txs(
    keypairs: &Vec<Keypair>,
    rpc_client: &RpcClient,
    mint: Pubkey,
) -> Result<()> {
    let mut latest_hash = LatestHash(rpc_client.get_latest_blockhash().await?, Instant::now());

    let mut ftrs: FuturesUnordered<_> = FuturesUnordered::new();

    (0..40).for_each(|_| {
        ftrs.push(create_transaction_future(
            gen_random_sol_tx(keypairs, &latest_hash).unwrap(),
            &rpc_client,
        ));
        ftrs.push(create_transaction_future(
            gen_random_token_tx(keypairs, &mint, &latest_hash).unwrap(),
            &rpc_client,
        ));
    });
    let mut swap = true;
    while let Some(txresult) = ftrs.next().await {
        match txresult {
            Ok(sig) => info!(
                "successful tx https://explorer.solana.com/tx/{}?cluster=custom",
                sig
            ),
            Err(err) => error!("tx error {}", err),
        }
        latest_hash.refresh(rpc_client).await?;
        match swap {
            true => {
                ftrs.push(create_transaction_future(
                    gen_random_sol_tx(keypairs, &latest_hash).unwrap(),
                    &rpc_client,
                ));
                swap = false
            }
            false => {
                ftrs.push(create_transaction_future(
                    gen_random_token_tx(keypairs, &mint, &latest_hash).unwrap(),
                    &rpc_client,
                ));
                swap = true
            }
        }
    }

    Ok(())
}

fn create_transaction_future(
    tx: Transaction,
    client: &RpcClient,
) -> impl Future<Output = Result<Signature, ClientError>> {
    async move { client.send_transaction(&tx).await }
}

fn create_confirmed_transaction_future(
    tx: Transaction,
    client: &RpcClient,
) -> impl Future<Output = Result<Signature, ClientError>> {
    async move { client.send_and_confirm_transaction(&tx).await }
}

fn gen_random_sol_tx(keypairs: &Vec<Keypair>, latest_hash: &LatestHash) -> Result<Transaction> {
    let mut rng = rand::rng();
    let from_keypair = keypairs.choose(&mut rng).unwrap(); // only fails if the vec is empty
    let to_keypair = keypairs.choose(&mut rng).unwrap();

    // random number between 5000 and 10000 laports, 5000 is minimum for tx fee
    let amount = rng.random_range(5000..10000);

    let instruction =
        system_instruction::transfer(&from_keypair.pubkey(), &to_keypair.pubkey(), amount);

    Ok(Transaction::new_signed_with_payer(
        &[instruction],
        Some(&from_keypair.pubkey()),
        &[from_keypair],
        latest_hash.0,
    ))
}

fn gen_random_token_tx(
    keypairs: &Vec<Keypair>,
    mint_pubkey: &Pubkey,
    latest_hash: &LatestHash,
) -> Result<Transaction> {
    let mut rng = rand::rng();
    let from_keypair = keypairs.choose(&mut rng).unwrap(); // only fails if the vec is empty
    let to_keypair = keypairs.choose(&mut rng).unwrap();

    // random number between 5000 and 10000 laports, 5000 is minimum for tx fee
    let amount = rng.random_range(69..420);
    let from_token_account = get_associated_token_address_with_program_id(
        &from_keypair.pubkey(),
        mint_pubkey,
        &spl_token_2022::id(),
    );
    let to_token_account = get_associated_token_address_with_program_id(
        &to_keypair.pubkey(),
        mint_pubkey,
        &spl_token_2022::id(),
    );

    let transfer_ix = spl_token_2022::instruction::transfer_checked(
        &spl_token_2022::id(),
        &from_token_account,
        &*mint_pubkey,
        &to_token_account,
        &from_keypair.pubkey(),
        &[&from_keypair.pubkey()],
        amount,
        0,
    )
    .unwrap();
    let transaction = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&from_keypair.pubkey()),
        &[from_keypair],
        latest_hash.0,
    );
    Ok(transaction)
}

async fn setup_token(keypairs: &Vec<Keypair>, rpc_client: &RpcClient) -> Result<Pubkey> {
    let mint_authority = keypairs.first().unwrap();
    let mint_keypair = Keypair::new();

    // Create and initialize mint account
    let mint_space = Mint::LEN;
    let lamports = rpc_client
        .get_minimum_balance_for_rent_exemption(mint_space)
        .await?;

    loop {
        let create_mint_account_ix = system_instruction::create_account(
            &mint_authority.pubkey(),
            &mint_keypair.pubkey(),
            lamports,
            mint_space as u64,
            &spl_token_2022::id(),
        );
        let initialize_mint_ix = initialize_mint(
            &spl_token_2022::id(),
            &mint_keypair.pubkey(),
            &mint_authority.pubkey(),
            None,
            0,
        )
        .unwrap();

        let transaction = Transaction::new_signed_with_payer(
            &[create_mint_account_ix, initialize_mint_ix],
            Some(&mint_authority.pubkey()),
            &[mint_authority, &mint_keypair],
            rpc_client.get_latest_blockhash().await?,
        );
        if let Err(error) = rpc_client.send_and_confirm_transaction(&transaction).await {
            error!("error initializing mint {}", error);
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            info!("mint {}", mint_keypair.pubkey());
            break;
        }
    }

    let mut ftrs = FuturesUnordered::new();
    let mut atas = vec![];
    let latest_hash = rpc_client.get_latest_blockhash().await?;
    // Set up associated token accounts and mint tokens to each
    for keypair in keypairs {
        let user_pubkey = keypair.pubkey();

        let associated_token_account = get_associated_token_address_with_program_id(
            &user_pubkey,
            &mint_keypair.pubkey(),
            &spl_token_2022::id(),
        );

        atas.push(associated_token_account);
        // Check if associated token account exists
        let exists = match rpc_client.get_account(&associated_token_account).await {
            Ok(_account) => true,
            Err(e) if e.to_string().contains("AccountNotFound") => false,
            Err(e) => return Err(anyhow::anyhow!(e.to_string())),
        };

        if !exists {
            // Create associated token account
            info!("creating ata {}", &associated_token_account);
            // tokio::time::sleep(Duration::from_secs(1)).await;
            let create_associated_token_account_ix = create_associated_token_account_idempotent(
                &user_pubkey,
                &user_pubkey,
                &mint_keypair.pubkey(),
                &spl_token_2022::id(),
            );
            let transaction = Transaction::new_signed_with_payer(
                &[create_associated_token_account_ix],
                Some(&user_pubkey),
                &[keypair],
                latest_hash,
            );
            ftrs.push(create_confirmed_transaction_future(transaction, rpc_client));
            // match rpc_client.send_and_confirm_transaction(&transaction).await {
            //     Ok(sig) => info!("ata created {}", sig),
            //     Err(err) => error!("{}", err.to_string()),
            // }
            // let _ = rpc_client.get_signature_status(&signature).await?;
        }
    }

    while let Some(txresult) = ftrs.next().await {
        match txresult {
            Ok(sig) => {}
            Err(err) => error!("{}", err.to_string()),
        }
    }
    ftrs.clear();
    for ata in atas {
        // Mint tokens to the associated token account
        let amount = 10000;
        let mint_to_ix = mint_to(
            &spl_token_2022::id(),
            &mint_keypair.pubkey(),
            &ata,
            &mint_authority.pubkey(),
            &[&mint_authority.pubkey()],
            amount,
        )
        .unwrap();

        let transaction = Transaction::new_signed_with_payer(
            &[mint_to_ix],
            Some(&mint_authority.pubkey()),
            &[mint_authority],
            rpc_client.get_latest_blockhash().await?,
        );

        ftrs.push(create_confirmed_transaction_future(transaction, rpc_client));
        //  loop {
        //      tokio::time::sleep(Duration::from_secs(2)).await;
        //      match rpc_client.send_transaction(&transaction).await {
        //          Ok(sig) => {
        //              info!("minted https://explorer.solana.com/tx/{}?cluster=custom", sig);
        //              break
        //          },
        //          Err(err) => {
        //              error!("Mint failed {}", err.to_string());
        //              continue;
        //          },
        //      }
        //  }
    }
    while let Some(txresult) = ftrs.next().await {
        match txresult {
            Ok(sig) => {
                info!(
                    "minted https://explorer.solana.com/tx/{}?cluster=custom",
                    sig
                );
            }
            Err(err) => error!("{}", err.to_string()),
        }
    }

    Ok(mint_keypair.pubkey())
}
