use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Bus, Config, Proof},
};
use ore_utils::AccountDeserialize;
use rand::Rng;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;

use crate::{
    args::MineArgs,
    send_and_confirm::ComputeBudget,
    utils::{
        amount_u64_to_string, get_clock, get_config, get_updated_proof_with_authority, proof_pubkey,
    },
    Miner,
};

// Define a constant for maximum retry attempts
const MAX_RETRIES: u8 = 3;

impl Miner {
    pub async fn mine(&self, args: MineArgs) {
        // Open account, if needed.
        let signer = self.signer();
        self.open().await;

        // Check num threads
        self.check_num_cores(args.cores);

        // Start mining loop
        let mut last_hash_at = 0;
        let mut last_balance = 0;
        loop {
            // Fetch proof
            let config = get_config(&self.rpc_client).await;
            let proof =
                get_updated_proof_with_authority(&self.rpc_client, signer.pubkey(), last_hash_at)
                    .await;
            println!(
                "\n\nStake: {} ORE\n{}  Multiplier: {:12}x",
                amount_u64_to_string(proof.balance),
                if last_hash_at.gt(&0) {
                    format!(
                        "  Change: {} ORE\n",
                        amount_u64_to_string(proof.balance.saturating_sub(last_balance))
                    )
                } else {
                    "".to_string()
                },
                calculate_multiplier(proof.balance, config.top_balance)
            );
            last_hash_at = proof.last_hash_at;
            last_balance = proof.balance;

            // Calculate cutoff time
            let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;

            // Run drillx
            let solution =
                Self::find_hash_par(proof, cutoff_time, args.cores, config.min_difficulty as u32)
                    .await;

            // Build instruction set
            let mut ixs = vec![ore_api::instruction::auth(proof_pubkey(signer.pubkey()))];
            let mut compute_budget = 500_000;
            if self.should_reset(config).await && rand::thread_rng().gen_range(0..100).eq(&0) {
                compute_budget += 100_000;
                ixs.push(ore_api::instruction::reset(signer.pubkey()));
            }

            // Build mine ix
            ixs.push(ore_api::instruction::mine(
                signer.pubkey(),
                signer.pubkey(),
                self.find_bus().await,
                solution,
            ));

            // Retry mechanism for the transaction submission
            let mut attempts = 0;
            while attempts < MAX_RETRIES {
                let result = self
                    .send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false)
                    .await;

                match result {
                    Ok(_) => {
                        // Success handling
                        break;
                    }
                    Err(e) => {
                        attempts += 1;
                        eprintln!(
                            "Attempt {}: ERROR: Failed to process transaction: {:?}",
                            attempts, e
                        );
                        if attempts >= MAX_RETRIES {
                            eprintln!("Max retries reached. Aborting...");
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn find_hash_par(
        proof: Proof,
        cutoff_time: u64,
        cores: u64,
        min_difficulty: u32,
    ) -> Solution {
        // Dispatch job to each thread
        let progress_bar = Arc::new(spinner::new_progress_bar());
        let global_best_difficulty = Arc::new(AtomicU32::new(0));
        let global_total_hashes = Arc::new(AtomicU64::new(0));
        progress_bar.set_message("Mining...");
        let core_ids = core_affinity::get_core_ids().unwrap();

        let start_time = Instant::now();

        let handles: Vec<_> = core_ids
            .into_iter()
            .map(|i| {
                let global_best_difficulty = Arc::clone(&global_best_difficulty);
                let global_total_hashes = Arc::clone(&global_total_hashes);
                std::thread::spawn({
                    let proof = proof.clone();
                    let progress_bar = progress_bar.clone();
                    let mut memory = equix::SolverMemory::new();
                    move || {
                        // Return if core should not be used
                        if (i.id as u64).ge(&cores) {
                            return (0, 0, Hash::default());
                        }

                        // Pin to core
                        let _ = core_affinity::set_for_current(i);

                        // Start hashing
                        let timer = Instant::now();
                        let mut nonce = u64::MAX.saturating_div(cores).saturating_mul(i.id as u64);
                        let mut best_nonce = nonce;
                        let mut best_difficulty = 0;
                        let mut best_hash = Hash::default();
                        loop {
                            // Create hash
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                if difficulty > best_difficulty {
                                    best_nonce = nonce;
                                    best_difficulty = difficulty;
                                    best_hash = hx;
                                    if difficulty > global_best_difficulty.load(Ordering::Relaxed) {
                                        global_best_difficulty.store(difficulty, Ordering::Relaxed);
                                    }
                                }
                            }

                            // Increment total hash counter
                            global_total_hashes.fetch_add(1, Ordering::Relaxed);

                            // Exit if time has elapsed
                            if nonce % 100 == 0 {
                                let global_best_difficulty =
                                    global_best_difficulty.load(Ordering::Relaxed);
                                let total_hashes = global_total_hashes.load(Ordering::Relaxed);
                                let elapsed_time = start_time.elapsed().as_secs_f64();
                                let hash_rate = total_hashes as f64 / elapsed_time;

                                if timer.elapsed().as_secs() >= cutoff_time {
                                    if i.id == 0 {
                                        progress_bar.set_message(format!(
                                            "Mining... (difficulty {}, time {}, {:.2} H/s)",
                                            global_best_difficulty,
                                            format_duration(
                                                cutoff_time
                                                    .saturating_sub(timer.elapsed().as_secs())
                                                    as u32
                                            ),
                                            hash_rate,
                                        ));
                                    }
                                    if global_best_difficulty >= min_difficulty {
                                        // Mine until min difficulty has been met
                                        break;
                                    }
                                } else if i.id == 0 {
                                    progress_bar.set_message(format!(
                                        "Mining... (difficulty {}, time {}, {:.2} H/s)",
                                        global_best_difficulty,
                                        format_duration(
                                            cutoff_time.saturating_sub(timer.elapsed().as_secs())
                                                as u32
                                        ),
                                        hash_rate,
                                    ));
                                }
                            }

                            // Increment nonce
                            nonce += 1;
                        }

                        // Return the best nonce
                        (best_nonce, best_difficulty, best_hash)
                    }
                })
            })
            .collect();

        // Join handles and return best nonce
        let mut best_nonce = 0;
        let mut best_difficulty = 0;
        let mut best_hash = Hash::default();
        for h in handles {
            if let Ok((nonce, difficulty, hash)) = h.join() {
                if difficulty > best_difficulty {
                    best_difficulty = difficulty;
                    best_nonce = nonce;
                    best_hash = hash;
                }
            }
        }

        // Calculate final hash rate
        let total_hashes = global_total_hashes.load(Ordering::Relaxed);
        let elapsed_time = start_time.elapsed().as_secs_f64();
        let hash_rate = total_hashes as f64 / elapsed_time;

        // Update log with final hash rate
        progress_bar.finish_with_message(format!(
            "Best hash: {} (difficulty {}, {:.2} H/s)",
            bs58::encode(best_hash.h).into_string(),
            best_difficulty,
            hash_rate,
        ));

        Solution::new(best_hash.d, best_nonce.to_le_bytes())
    }

    pub fn check_num_cores(&self, cores: u64) {
        let num_cores = num_cpus::get() as u64;
        if cores > num_cores {
            println!(
                "{} Cannot exceed available cores ({})",
                "WARNING".bold().yellow(),
                num_cores
            );
        }
    }

    async fn should_reset(&self, config: Config) -> bool {
        let clock = get_clock(&self.rpc_client).await;
        config
            .last_reset_at
            .saturating_add(EPOCH_DURATION)
            .saturating_sub(5) // Buffer
            .le(&clock.unix_timestamp)
    }

    async fn get_cutoff(&self, proof: Proof, buffer_time: u64) -> u64 {
        let clock = get_clock(&self.rpc_client).await;
        proof
            .last_hash_at
            .saturating_add(60)
            .saturating_sub(buffer_time as i64)
            .saturating_sub(clock.unix_timestamp)
            .max(0) as u64
    }

    async fn find_bus(&self) -> Pubkey {
        // Fetch the bus with the largest balance
        if let Ok(accounts) = self.rpc_client.get_multiple_accounts(&BUS_ADDRESSES).await {
            let mut top_bus_balance: u64 = 0;
            let mut top_bus = BUS_ADDRESSES[0];
            for account in accounts {
                if let Some(account) = account {
                    if let Ok(bus) = Bus::try_from_bytes(&account.data) {
                        if bus.rewards > top_bus_balance {
                            top_bus_balance = bus.rewards;
                            top_bus = BUS_ADDRESSES[bus.id as usize];
                        }
                    }
                }
            }
            return top_bus;
        }

        // Otherwise return a random bus
        let i = rand::thread_rng().gen_range(0..BUS_COUNT);
        BUS_ADDRESSES[i]
    }
}

fn calculate_multiplier(balance: u64, top_balance: u64) -> f64 {
    1.0 + (balance as f64 / top_balance as f64).min(1.0f64)
}

fn format_duration(seconds: u32) -> String {
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    format!("{:02}:{:02}", minutes, remaining_seconds)
}
