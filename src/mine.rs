use std::sync::Arc;
use std::time::{Instant, Duration};
use humantime::format_duration;
use systemstat::{System, Platform};
use chrono::prelude::*;

use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore::{self, state::Proof, BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION, ONE_DAY};

use rand::Rng;
use solana_program::{
	pubkey::Pubkey,
    native_token::{lamports_to_sol, sol_to_lamports},
};

use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;
// use spl_token::amount_to_ui_amount;

use crate::{
    args::MineArgs,
	send_and_confirm::ComputeBudget,
    utils::{amount_u64_to_f64, get_clock, get_config, get_proof},
    Miner,
};

impl Miner {
    pub async fn mine(&self, args: MineArgs) {
		const MIN_SOL_BALANCE: f64 = 0.005;

		// Register, if needed.
        let signer = self.signer();
        self.register().await;

		let sys = System::new();

        // Check num threads
        self.check_num_cores(args.threads);
		let mining_start_time = Instant::now();
		let mut pass=1;
		let mut current_sol_balance: f64;
		let mut current_staked_balance: f64;

		// let mut starting_sol_balance: f64 = 0.0;
		// let mut starting_staked_balance: f64 = 0.0;

		let mut last_sol_balance: f64 = 0.0;
		let mut last_staked_balance: f64 = 0.0;

		let mut session_ore_mined: f64 = 0.0;
		let mut session_sol_used: f64 = 0.0;

        // Start mining loop
        loop {
			let pass_start_time = Instant::now();

            // Fetch proof
            let proof = get_proof(&self.rpc_client, signer.pubkey()).await;

			// Calc cutoff time
            let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;

			// Determine Wallet ORE & SOL Balances
			current_sol_balance=self.get_sol_balance(false).await;
			current_staked_balance=amount_u64_to_f64(proof.balance);

			// Determine if Staked ORE can be withdrawing without penalty or if ORE will be burned
			let clock = get_clock(&self.rpc_client).await;
			let t = proof.last_claim_at.saturating_add(ONE_DAY);
			// let mut claimable=true;
			let mut claim_text="No Withdrawal Penalty".green().to_string();
			let last_claimed = (clock.unix_timestamp.saturating_sub(proof.last_claim_at) as f64) / 60f64 / 64f64;
			if clock.unix_timestamp.lt(&t) {
				// claimable=false;
				// let burn_amount = proof.balance
				// 	.saturating_mul(t.saturating_sub(clock.unix_timestamp) as u64)
				// 	.saturating_div(ONE_DAY as u64);
				let mins_to_go = t.saturating_sub(clock.unix_timestamp).saturating_div(60);
				claim_text = format!("{} {} {}",
						"Withdrawal Penalty for".bold().red(),
						mins_to_go.to_string().bold().red(),
						"mins".bold().red(),
					);
			}

			// Summarize the results of the previous mining pass
			if pass>1 {
				// Add the difference in staked ore from the previous pass to the session_ore_mined tally
				let mut last_pass_ore_mined=current_staked_balance-last_staked_balance;
				// If ore has been unstaked, then this value will be wrong for last pass so ignore it
				if last_pass_ore_mined<0.0 {
					last_pass_ore_mined=0.0;
				}
				// Not sure how to detect is additional ore has been staked
				// possible to check with proof last claimed > last pass start time?
				session_ore_mined+=last_pass_ore_mined;	// Update the session ore mined tally

				// Add the difference in sol from the previous pass to the session_sol_used tally
				let mut last_pass_sol_used=current_sol_balance-last_sol_balance;
				// Sol has been added to wallet so disregard the last passed sol_used as it is incorrect
				if last_pass_sol_used>0.0 {
					last_pass_sol_used=-0.0;
				}
				// not sure how to detect a change in sol level after the start of the last pass that is not just a transaction fee.
				session_sol_used-=last_pass_sol_used;	// Update the session sol used tally

				println!("    - Mined: {:.11}\t      Cost: {:.9}\tSession: {:.11} ORE\t{:.9} SOL",
					last_pass_ore_mined,
					last_pass_sol_used,
					session_ore_mined,
					session_sol_used,
				);
				println!("\n{}\n", ("====================================================================================================").to_string().dimmed());
			}

			// Store this pass's sol/staked balances for use in the next pass
			last_sol_balance=current_sol_balance;
			last_staked_balance=current_staked_balance;

			// Lookup CPU stats for 1min, 5 mins and 15 mins
			let mut load_avg_1min: f32=0.0;
			let mut load_avg_5min: f32=0.0;
			let mut load_avg_15min: f32=0.0;
			match sys.load_average() {
				Ok(load_avg) => {
					load_avg_1min=load_avg.one;			// 1 min
					load_avg_5min=load_avg.five;		// 5 min
					load_avg_15min=load_avg.fifteen;	// 15 min
				}
				Err(err) => eprintln!("Error (load_average): {}", err),
			}
			// This will not report anything if in WSL2 on windows
			let cpu_temp: f32;
			match sys.cpu_temp() {
				Ok(t) => { cpu_temp=t; },
				Err(_err) => { cpu_temp=-99.0; },
					// eprintln!("Error (cpu_temp): {}", err),
			};

			// Write log details to console to summarize this miner's wallet
			println!("Pass {} started at {}\tMined for {}\tCPU: {}°C   {:.2}/{:.2}/{:.2}",
				pass,
				Utc::now().format("%H:%M:%S on %Y-%m-%d").to_string(),
				format_duration(Duration::from_secs(mining_start_time.elapsed().as_secs())),
				cpu_temp.to_string(),
				load_avg_1min,
				load_avg_5min,
				load_avg_15min,
			);
			println!("        Currently staked ORE: {:.11}\tWallet SOL:  {:.9}\tLast Withdrawal: {:.1} hours ago {}",
				current_staked_balance,
				current_sol_balance,
				last_claimed,
				claim_text,
            );

			if current_sol_balance<MIN_SOL_BALANCE {
				// Pause mining for one minute - awaitin gSOL to be added to miner's wallet
				let progress_bar = Arc::new(spinner::new_progress_bar());
				for _ in 0..60 {
					progress_bar.set_message(format!("[{}{}] {}",
						(60-pass_start_time.elapsed().as_secs()).to_string().dimmed(),
						("s to go").dimmed(),
						("Not enough sol in wallet. Please deposit more to continue mining after the timeout.").yellow(),
					));
					std::thread::sleep(Duration::from_millis(1000));
				}
				progress_bar.finish_with_message(format!("[{}{}] {}",
					(60-pass_start_time.elapsed().as_secs()).to_string().dimmed(),
					("s").dimmed(),
					("Not enough sol in wallet. Please deposit more to continue mining.").yellow(),
				));

			} else {
				// Run drillx
				let solution = self.find_hash_par(proof, cutoff_time, args.threads).await;

				// Submit most difficult hash
				let mut ixs = vec![];
				if self.needs_reset().await {
					ixs.push(ore::instruction::reset(signer.pubkey()));
				}
				ixs.push(ore::instruction::mine(
					signer.pubkey(),
					find_bus(),
					solution,
				));
				self.send_and_confirm(&ixs, ComputeBudget::Fixed(500_000), false, true)
					.await
					.ok();
			}

			// Log how long this pass took to complete
			print!("  [{}{}] Completed",
				pass_start_time.elapsed().as_secs().to_string().dimmed(),
				"s".dimmed(),
			);
			pass+=1;
        }
    }

    async fn find_hash_par(&self, proof: Proof, cutoff_time: u64, threads: u64) -> Solution {
        // Dispatch job to each thread
		let timer = Instant::now();
		let progress_bar = Arc::new(spinner::new_progress_bar());
        progress_bar.set_message(format!("[{}s to go] Mining...", cutoff_time));
		let handles: Vec<_> = (0..threads)
            .map(|i| {
                std::thread::spawn({
                    let proof = proof.clone();
                    let progress_bar = progress_bar.clone();
                    let mut memory = equix::SolverMemory::new();
                    move || {
                        let mut nonce = u64::MAX.saturating_div(threads).saturating_mul(i);
                        let mut best_nonce = nonce;
                        let mut best_difficulty = 0;
                        let mut best_hash = Hash::default();
						let mut last_elapsed:u64 = 0;
                        loop {
		                    // Create hash
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                if difficulty.gt(&best_difficulty) {
                                    best_nonce = nonce;
                                    best_difficulty = difficulty;
                                    best_hash = hx;
                                }
                            }

                            // Exit if time has elapsed
                            if nonce % 100 == 0 {
                                if timer.elapsed().as_secs().ge(&cutoff_time) {
                                    if best_difficulty.gt(&ore::MIN_DIFFICULTY) {
                                        // Mine until min difficulty has been met
                                        break;
                                    }
                                } else if i == 0 {
									let next_elapsed=timer.elapsed().as_secs();
									if next_elapsed != last_elapsed {
										progress_bar.set_message(format!(
											"[{}{}] Mining... {} {}",
											cutoff_time.saturating_sub(next_elapsed).to_string().dimmed(),
											"s to go".dimmed(),
											"Difficulty so far:".dimmed(),
											best_difficulty.to_string().yellow(),
										));
										last_elapsed=next_elapsed;
									}
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

        // Update log
		progress_bar.finish_with_message(format!(
            "[{}{}] Difficulty: {}\t    Hash: {} ",
			timer.elapsed().as_secs().to_string().dimmed(),
			"s".dimmed(),
            best_difficulty.to_string().bold().yellow(),
            bs58::encode(best_hash.h).into_string().dimmed(),
        ));

        Solution::new(best_hash.d, best_nonce.to_le_bytes())
    }

    pub fn check_num_cores(&self, threads: u64) {
        // Check num threads
        let num_cores = num_cpus::get() as u64;
        if threads.gt(&num_cores) {
            println!(
                "{} Number of threads ({}) exceeds available cores ({})",
                "WARNING".bold().yellow(),
                threads,
                num_cores
            );
        }
    }

    async fn needs_reset(&self) -> bool {
        let clock = get_clock(&self.rpc_client).await;
        let config = get_config(&self.rpc_client).await;
        config
            .last_reset_at
            .saturating_add(EPOCH_DURATION)
            .saturating_sub(5) // Buffer
            .le(&clock.unix_timestamp)
    }

    async fn get_cutoff(&self, proof: Proof, buffer_time: u64) -> u64 {
        let clock = get_clock(&self.rpc_client).await;
        let mut retval=proof.last_hash_at
            .saturating_add(60)
            .saturating_sub(buffer_time as i64)
            .saturating_sub(clock.unix_timestamp)
            .max(0) as u64;
		if retval==0 {
			retval=(60 as i64).saturating_sub(buffer_time as i64).max(0) as u64;
		}
		return retval;
    }

	async fn get_sol_balance(&self, panic: bool) -> f64 {
		const MIN_SOL_BALANCE: f64 = 0.005;
		let signer = self.signer();
		let client = self.rpc_client.clone();

		// Return error, if balance is zero
		if let Ok(lamports_balance) = client.get_balance(&signer.pubkey()).await {
			let sol_balance:f64 = lamports_to_sol(lamports_balance);
			if lamports_balance <= sol_to_lamports(MIN_SOL_BALANCE) {
				if panic {
					panic!(
						"{} Insufficient balance: {} SOL\nPlease top up with at least {} SOL",
						"ERROR".bold().red(),
						sol_balance,
						MIN_SOL_BALANCE
					);
				}
			}
			return sol_balance
		} else {
			if panic {
				panic!(
					"{} Failed to lookup sol balance",
					"ERROR".bold().red(),
				);
			} else {
				return 0.0 as f64
			}
		}
	}
}

// TODO Pick a better strategy (avoid draining bus)
fn find_bus() -> Pubkey {
    let i = rand::thread_rng().gen_range(0..BUS_COUNT);
    BUS_ADDRESSES[i]
}
