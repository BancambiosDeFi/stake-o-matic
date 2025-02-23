use {
    crate::{
        generic_stake_pool::*,
        rpc_client_utils::{get_all_stake, send_and_confirm_transactions},
    },
    borsh::BorshDeserialize,
    log::*,
    solana_client::{rpc_client::RpcClient, rpc_response::StakeActivationState},
    solana_sdk::{
        borsh::try_from_slice_unchecked,
        native_token::{Sol, LAMPORTS_PER_SOL},
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    },
    solana_stake_program::{stake_instruction, stake_state::StakeState},
    spl_stake_pool::{
        self, find_stake_program_address, find_transient_stake_program_address,
        stake_program::split_only,
        state::{StakePool, StakeStatus, ValidatorList},
    },
    std::{collections::HashSet, error, mem},
};

/// Minimum amount of lamports in a validator stake account, on top of the
/// rent-exempt amount
pub const MIN_STAKE_ACCOUNT_BALANCE: u64 = LAMPORTS_PER_SOL;

/// Minimum amount of lamports in the stake pool reserve, on top of the rent-exempt
/// amount
pub const MIN_STAKE_RESERVE_BALANCE: u64 = 1;

/// Don't bother adjusting stake if less than this amount of lamports will be affected
/// (must be >= MIN_STAKE_ACCOUNT_BALANCE)
const MIN_STAKE_CHANGE_AMOUNT: u64 = MIN_STAKE_ACCOUNT_BALANCE;

fn get_minimum_stake_balance_for_rent_exemption(
    rpc_client: &RpcClient,
) -> Result<u64, Box<dyn error::Error>> {
    rpc_client
        .get_minimum_balance_for_rent_exemption(mem::size_of::<StakeState>())
        .map_err(|err| format!("Error fetching rent exemption: {}", err).into())
}

/// Seed for the transient stake account used by the staker
fn staker_transient_stake_address_seed(vote_address: Pubkey) -> String {
    format!("{}", vote_address)[..32].to_string()
}

/// Staker's transient stake account
///
/// When adding a new validator to the pool, we must create a validator stake
/// account with the rent-exempt amount + MIN_STAKE_ACCOUNT_BALANCE, and delegate
/// the account to the appropriate vote address.  Once the stake is active, we
/// can add it to the pool.
///
/// Separately, the stake pool enforces that a new validator stake account must
/// have exactly the rent-exempt amount + MIN_STAKE_ACCOUNT_BALANCE as its balance.
/// Since the validator stake account was staked for an epoch, it earned rewards during
/// that epoch, bringing it over the enforced lamport amount. The extra amount
/// is split into a transient stake account defined by this function.
fn staker_transient_stake_address(authorized_staker: Pubkey, vote_address: Pubkey) -> Pubkey {
    Pubkey::create_with_seed(
        &authorized_staker,
        &staker_transient_stake_address_seed(vote_address),
        &solana_stake_program::id(),
    )
    .unwrap()
}

#[derive(Debug)]
pub struct StakePoolOMatic {
    authorized_staker: Keypair,
    baseline_stake_amount: u64,
    stake_pool_address: Pubkey,
    stake_pool: StakePool,
    validator_list: ValidatorList,
}

pub fn new(
    rpc_client: &RpcClient,
    authorized_staker: Keypair,
    stake_pool_address: Pubkey,
    baseline_stake_amount: u64,
) -> Result<StakePoolOMatic, Box<dyn error::Error>> {
    if baseline_stake_amount < MIN_STAKE_CHANGE_AMOUNT {
        return Err(format!(
            "baseline stake amount too small: {}",
            Sol(baseline_stake_amount)
        )
        .into());
    }

    let account_data = rpc_client.get_account_data(&stake_pool_address)?;
    let stake_pool = StakePool::try_from_slice(account_data.as_slice())
        .map_err(|err| format!("Invalid stake pool {}: {}", stake_pool_address, err))?;
    let account_data = rpc_client.get_account_data(&stake_pool.validator_list)?;
    let validator_list = try_from_slice_unchecked::<ValidatorList>(&account_data.as_slice())
        .map_err(|err| {
            format!(
                "Invalid validator list {}: {}",
                stake_pool.validator_list, err
            )
        })?;

    Ok(StakePoolOMatic {
        authorized_staker,
        baseline_stake_amount,
        stake_pool_address,
        stake_pool,
        validator_list,
    })
}

impl StakePoolOMatic {
    /// Perform the double update, required at the start of an epoch:
    /// * call into the stake pool program to update the accounting of lamports
    /// * update the StakePool and ValidatorList objects based on the accounting
    pub fn epoch_update(&mut self, rpc_client: &RpcClient) -> Result<(), Box<dyn error::Error>> {
        update_stake_pool(
            rpc_client,
            &self.authorized_staker,
            &self.stake_pool_address,
            &self.stake_pool,
            &self.validator_list,
        )?;
        self.update(rpc_client)?;
        Ok(())
    }

    /// Update the StakePoolOMatic instance with the current StakePool and ValidatorList
    /// from the network.
    pub fn update(&mut self, rpc_client: &RpcClient) -> Result<(), Box<dyn error::Error>> {
        let account_data = rpc_client.get_account_data(&self.stake_pool_address)?;
        self.stake_pool = StakePool::try_from_slice(account_data.as_slice())
            .map_err(|err| format!("Invalid stake pool {}: {}", self.stake_pool_address, err))?;
        let account_data = rpc_client.get_account_data(&self.stake_pool.validator_list)?;
        self.validator_list = try_from_slice_unchecked::<ValidatorList>(&account_data.as_slice())
            .map_err(|err| {
            format!(
                "Invalid validator list {}: {}",
                self.stake_pool.validator_list, err
            )
        })?;
        Ok(())
    }
}

impl GenericStakePool for StakePoolOMatic {
    fn apply(
        &mut self,
        rpc_client: &RpcClient,
        dry_run: bool,
        desired_validator_stake: &[ValidatorStake],
    ) -> Result<(Vec<String>, bool), Box<dyn error::Error>> {
        if dry_run {
            return Err("dryrun not supported".into());
        }

        let mut bonus_stake_node_count = 0;
        let mut baseline_stake_node_count = 0;

        // used to find any validators that should be removed from the stake pool
        let mut inuse_vote_addresses = HashSet::new();

        for ValidatorStake {
            stake_state,
            vote_address,
            ..
        } in desired_validator_stake
        {
            inuse_vote_addresses.insert(*vote_address);

            match stake_state {
                ValidatorStakeState::Bonus => bonus_stake_node_count += 1,
                ValidatorStakeState::Baseline => baseline_stake_node_count += 1,
                ValidatorStakeState::None => (),
            }
        }

        info!("Withdraw inactive transient stake accounts to the staker");
        withdraw_inactive_stakes_to_staker(rpc_client, &self.authorized_staker)?;

        info!("Update the stake pool, merging transient stakes and orphaned accounts");
        self.epoch_update(rpc_client)?;

        let all_vote_addresses: HashSet<Pubkey> = self
            .validator_list
            .validators
            .iter()
            .map(|x| x.vote_account_address)
            .collect();
        info!("Remove validators no longer present in the desired list");
        remove_validators_from_pool(
            rpc_client,
            &self.authorized_staker,
            &self.stake_pool_address,
            &self.stake_pool,
            &self.validator_list,
            &all_vote_addresses - &inuse_vote_addresses,
        )?;

        info!("Add new validators to pool if active");
        add_validators_to_pool(
            rpc_client,
            &self.authorized_staker,
            desired_validator_stake,
            &self.stake_pool_address,
            &self.stake_pool,
            &self.validator_list,
        )?;
        self.update(rpc_client)?;

        let mut busy_validators = HashSet::new();
        info!("Add unmerged transient stake accounts to the busy set");
        add_unmerged_transient_stake_accounts(
            rpc_client,
            desired_validator_stake,
            &self.stake_pool_address,
            &mut busy_validators,
        )?;

        info!("Create validator stake accounts if needed");
        create_validator_stake_accounts(
            rpc_client,
            &self.authorized_staker,
            desired_validator_stake,
            &self.stake_pool_address,
            &mut busy_validators,
        )?;

        let total_stake_amount = self.stake_pool.total_stake_lamports;
        info!(
            "Total stake pool balance minus required reserves: {}",
            Sol(total_stake_amount)
        );

        let total_baseline_stake_amount = baseline_stake_node_count * self.baseline_stake_amount;
        info!("Baseline node count: {}", baseline_stake_node_count);
        info!("Baseline stake amount: {}", Sol(self.baseline_stake_amount));
        info!(
            "Total baseline stake amount: {}",
            Sol(total_baseline_stake_amount)
        );

        if total_stake_amount < total_baseline_stake_amount {
            return Err("Not enough stake to cover the baseline".into());
        }

        info!("Bonus node count: {}", bonus_stake_node_count);
        let total_bonus_stake_amount =
            total_stake_amount.saturating_sub(total_baseline_stake_amount);
        info!(
            "Total bonus stake amount: {}",
            Sol(total_bonus_stake_amount)
        );

        let bonus_stake_amount = if bonus_stake_node_count == 0 {
            0
        } else {
            total_bonus_stake_amount / (bonus_stake_node_count as u64)
        };

        info!("Bonus stake amount: {}", Sol(bonus_stake_amount));

        let notes = vec![
            format!("Baseline stake amount: {}", Sol(self.baseline_stake_amount)),
            format!("Bonus stake amount: {}", Sol(bonus_stake_amount)),
        ];
        Ok((
            notes,
            distribute_validator_stake(
                rpc_client,
                &self.authorized_staker,
                &self.stake_pool_address,
                &self.stake_pool,
                &self.validator_list,
                desired_validator_stake
                    .iter()
                    .filter(|vs| !busy_validators.contains(&vs.identity))
                    .cloned(),
                self.baseline_stake_amount,
                bonus_stake_amount,
            )?,
        ))
    }
}

// Get the balance of a stake account excluding the reserve
fn get_available_stake_balance(
    rpc_client: &RpcClient,
    stake_address: Pubkey,
    reserve_stake_balance: u64,
) -> Result<u64, Box<dyn error::Error>> {
    let balance = rpc_client.get_balance(&stake_address).map_err(|err| {
        format!(
            "Unable to get stake account balance: {}: {}",
            stake_address, err
        )
    })?;
    if balance < reserve_stake_balance {
        Err(format!(
            "Stake account {} balance too low, {}. Minimum is {}",
            stake_address,
            Sol(balance),
            Sol(reserve_stake_balance)
        )
        .into())
    } else {
        Ok(balance.saturating_sub(reserve_stake_balance))
    }
}

/// Iterates through all possible transient stake accounts on the stake pool,
/// and if any is present, mark the validator as busy.
fn add_unmerged_transient_stake_accounts(
    rpc_client: &RpcClient,
    desired_validator_stake: &[ValidatorStake],
    stake_pool_address: &Pubkey,
    busy_validators: &mut HashSet<Pubkey>,
) -> Result<(), Box<dyn error::Error>> {
    for ValidatorStake {
        identity,
        vote_address,
        ..
    } in desired_validator_stake
    {
        let transient_stake_address = find_transient_stake_program_address(
            &spl_stake_pool::id(),
            vote_address,
            stake_pool_address,
        )
        .0;

        let transient_stake_account = rpc_client
            .get_account_with_commitment(&transient_stake_address, rpc_client.commitment())?
            .value;

        if transient_stake_account.is_some() {
            busy_validators.insert(*identity);
        }
    }
    Ok(())
}

/// Withdraw from inactive stake accounts owned by the staker, back to themself
///
/// The staker has two types of stake accounts to reclaim:
///
/// * removed validator stake accounts
/// * transient stake accounts created before adding, see `staker_transient_stake_address`
///   for more information
///
/// Every epoch, this function checks for any of these inactive stake accounts,
/// and withdraws the entirety back to the staker.
fn withdraw_inactive_stakes_to_staker(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
) -> Result<(), Box<dyn error::Error>> {
    let mut transactions = vec![];
    let (all_stake_addresses, _all_stake_total_amount) =
        get_all_stake(rpc_client, authorized_staker.pubkey())?;

    for stake_address in all_stake_addresses {
        let stake_account = rpc_client
            .get_account_with_commitment(&stake_address, rpc_client.commitment())?
            .value;

        if let Some(stake_account) = stake_account {
            // Check if the stake account is busy
            let stake_activation = rpc_client
                .get_stake_activation(stake_address, None)
                .map_err(|err| {
                    format!(
                        "Unable to get activation information for stake account: {}: {}",
                        stake_address, err
                    )
                })?;

            if stake_activation.state == StakeActivationState::Inactive {
                let stake_lamports = stake_account.lamports;
                transactions.push(Transaction::new_with_payer(
                    &[stake_instruction::withdraw(
                        &stake_address,
                        &authorized_staker.pubkey(),
                        &authorized_staker.pubkey(),
                        stake_lamports,
                        None,
                    )],
                    Some(&authorized_staker.pubkey()),
                ));
            } else {
                debug!("Staker's stake at {} not inactive, skipping", stake_address);
            }
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to add validators to the stake pool".into())
    } else {
        Ok(())
    }
}

/// Create and send all transactions to update the stake pool balances, required
/// once per epoch to perform any operations on the stake pool.
fn update_stake_pool(
    rpc_client: &RpcClient,
    payer: &Keypair,
    stake_pool_address: &Pubkey,
    stake_pool: &StakePool,
    validator_list: &ValidatorList,
) -> Result<(), Box<dyn error::Error>> {
    let instructions = spl_stake_pool::instruction::update_stake_pool(
        stake_pool,
        validator_list,
        stake_pool_address,
        false, // no_merge
    );

    let mut transactions: Vec<Transaction> = instructions
        .into_iter()
        .map(|i| Transaction::new_with_payer(&[i], Some(&payer.pubkey())))
        .collect();
    let update_balance_transaction = transactions.split_off(transactions.len() - 1);

    if !send_and_confirm_transactions(rpc_client, false, transactions, payer)?
        .failed
        .is_empty()
    {
        return Err("Failed to update stake pool".into());
    }

    if !send_and_confirm_transactions(rpc_client, false, update_balance_transaction, payer)?
        .failed
        .is_empty()
    {
        Err("Failed to update stake pool".into())
    } else {
        Ok(())
    }
}

/// Remove validators no longer present in the desired validator list
///
/// In order to properly remove a validator from the stake pool, their stake
/// account must first be reduced down to the minimum of rent-exemption + 1 SOL.
/// The staker will take control of the validator stake account on removal, so
/// this also deactivates the stake, to be reclaimed in the next epoch.
fn remove_validators_from_pool(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    stake_pool_address: &Pubkey,
    stake_pool: &StakePool,
    validator_list: &ValidatorList,
    remove_vote_addresses: HashSet<Pubkey>,
) -> Result<(), Box<dyn error::Error>> {
    let mut transactions = vec![];
    let stake_rent_exemption = get_minimum_stake_balance_for_rent_exemption(rpc_client)?;

    for vote_address in remove_vote_addresses {
        let validator_list_entry = validator_list.find(&vote_address);
        if let Some(validator_list_entry) = validator_list_entry {
            if validator_list_entry.status == StakeStatus::Active {
                let removed_stake_address = find_stake_program_address(
                    &spl_stake_pool::id(),
                    &vote_address,
                    stake_pool_address,
                )
                .0;
                let mut instructions = vec![];
                if validator_list_entry.stake_lamports > stake_rent_exemption {
                    instructions.push(
                        spl_stake_pool::instruction::decrease_validator_stake_with_vote(
                            stake_pool,
                            stake_pool_address,
                            &vote_address,
                            validator_list_entry.stake_lamports,
                        ),
                    );
                }

                instructions.push(
                    spl_stake_pool::instruction::remove_validator_from_pool_with_vote(
                        stake_pool,
                        stake_pool_address,
                        &vote_address,
                        &authorized_staker.pubkey(),
                    ),
                );
                instructions.push(stake_instruction::deactivate_stake(
                    &removed_stake_address,
                    &authorized_staker.pubkey(),
                ));
                transactions.push(Transaction::new_with_payer(
                    &instructions,
                    Some(&authorized_staker.pubkey()),
                ));
            } else {
                debug!("Validator {} already removed, ignoring", vote_address);
            }
        } else {
            warn!(
                "Validator {} not present in stake pool {}, ignoring removal",
                vote_address, stake_pool_address
            );
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to add validators to the stake pool".into())
    } else {
        Ok(())
    }
}

/// Add validator stake accounts that have been created and delegated, but not
/// included yet in the stake pool
fn add_validators_to_pool(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    desired_validator_stake: &[ValidatorStake],
    stake_pool_address: &Pubkey,
    stake_pool: &StakePool,
    validator_list: &ValidatorList,
) -> Result<(), Box<dyn error::Error>> {
    let mut transactions = vec![];
    let stake_rent_exemption = get_minimum_stake_balance_for_rent_exemption(rpc_client)?;
    let min_stake_account_balance = stake_rent_exemption + MIN_STAKE_ACCOUNT_BALANCE;

    for ValidatorStake {
        identity,
        vote_address,
        ..
    } in desired_validator_stake
    {
        if !validator_list.contains(vote_address) {
            let stake_address =
                find_stake_program_address(&spl_stake_pool::id(), vote_address, stake_pool_address)
                    .0;
            let stake_account = rpc_client
                .get_account_with_commitment(&stake_address, rpc_client.commitment())?
                .value;

            if let Some(stake_account) = stake_account {
                // Check if the stake account is busy
                let stake_activation = rpc_client
                    .get_stake_activation(stake_address, None)
                    .map_err(|err| {
                        format!(
                            "Unable to get activation information for stake account: {}: {}",
                            stake_address, err
                        )
                    })?;

                if stake_activation.state == StakeActivationState::Active {
                    info!("Adding validator {} to the pool", identity);
                    let mut instructions = vec![];
                    if stake_account.lamports > min_stake_account_balance {
                        let split_lamports = stake_account.lamports - min_stake_account_balance;
                        let transient_stake_address = staker_transient_stake_address(
                            authorized_staker.pubkey(),
                            *vote_address,
                        );
                        let transient_stake_address_seed =
                            staker_transient_stake_address_seed(*vote_address);
                        info!(
                            "Splitting {} lamports into staker account {}",
                            split_lamports, transient_stake_address
                        );
                        instructions.push(system_instruction::create_account_with_seed(
                            &authorized_staker.pubkey(),
                            &transient_stake_address,
                            &authorized_staker.pubkey(),
                            &transient_stake_address_seed,
                            stake_rent_exemption,
                            mem::size_of::<StakeState>() as u64,
                            &solana_stake_program::id(),
                        ));

                        instructions.push(split_only(
                            &stake_address,
                            &authorized_staker.pubkey(),
                            split_lamports,
                            &transient_stake_address,
                        ));
                        instructions.push(stake_instruction::deactivate_stake(
                            &transient_stake_address,
                            &authorized_staker.pubkey(),
                        ));
                    }
                    instructions.push(
                        spl_stake_pool::instruction::add_validator_to_pool_with_vote(
                            stake_pool,
                            stake_pool_address,
                            vote_address,
                        ),
                    );
                    transactions.push(Transaction::new_with_payer(
                        &instructions,
                        Some(&authorized_staker.pubkey()),
                    ));
                }
            }
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to add validators to the stake pool".into())
    } else {
        Ok(())
    }
}

/// Create validator stake accounts that are not currently included in the stake pool.
/// For any newly created account, the validator identity is added to the set of
/// busy validators.
fn create_validator_stake_accounts(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    desired_validator_stake: &[ValidatorStake],
    stake_pool_address: &Pubkey,
    busy_validators: &mut HashSet<Pubkey>,
) -> Result<(), Box<dyn error::Error>> {
    let mut staker_balance = rpc_client.get_balance(&authorized_staker.pubkey()).unwrap();
    info!("Staker available balance: {}", Sol(staker_balance));

    let stake_rent_exemption = get_minimum_stake_balance_for_rent_exemption(rpc_client)?;
    let min_stake_account_balance = stake_rent_exemption + MIN_STAKE_ACCOUNT_BALANCE;

    let mut transactions = vec![];
    for ValidatorStake {
        identity,
        vote_address,
        ..
    } in desired_validator_stake
    {
        let stake_address =
            find_stake_program_address(&spl_stake_pool::id(), vote_address, stake_pool_address).0;
        let stake_account = rpc_client
            .get_account_with_commitment(&stake_address, rpc_client.commitment())?
            .value;

        if stake_account.is_some() {
            // Check if the stake account is busy
            let stake_activation = rpc_client
                .get_stake_activation(stake_address, None)
                .map_err(|err| {
                    format!(
                        "Unable to get activation information for stake account: {}: {}",
                        stake_address, err
                    )
                })?;

            match stake_activation.state {
                StakeActivationState::Activating | StakeActivationState::Deactivating => {
                    warn!(
                        "Validator {} busy due to stake activation or deactivation of {}: {:?}",
                        identity, stake_address, stake_activation
                    );
                    busy_validators.insert(*identity);
                }
                StakeActivationState::Active => {}
                StakeActivationState::Inactive => {
                    warn!(
                        "Validator {} busy due to inactive stake {}: {:?}",
                        identity, stake_address, stake_activation
                    );
                    transactions.push(Transaction::new_with_payer(
                        &[stake_instruction::delegate_stake(
                            &stake_address,
                            &authorized_staker.pubkey(),
                            vote_address,
                        )],
                        Some(&authorized_staker.pubkey()),
                    ));
                    debug!(
                        "Activating stake account for validator {} ({})",
                        identity, stake_address
                    );
                    busy_validators.insert(*identity);
                }
            }
        } else {
            if staker_balance < min_stake_account_balance {
                // Try again next epoch
                warn!(
                    "Insufficient funds in reserve stake account to create stake account: {} required, {} balance",
                    Sol(min_stake_account_balance), Sol(staker_balance)
                );
            } else {
                // Create a stake account for the validator
                staker_balance -= min_stake_account_balance;

                let instruction =
                    spl_stake_pool::instruction::create_validator_stake_account_with_vote(
                        stake_pool_address,
                        &authorized_staker.pubkey(),
                        &authorized_staker.pubkey(),
                        vote_address,
                    );

                transactions.push(Transaction::new_with_payer(
                    &[instruction],
                    Some(&authorized_staker.pubkey()),
                ));
                info!(
                    "Creating stake account for validator {} ({})",
                    identity, stake_address
                );
            }
            warn!("Validator {} busy due to no stake account", identity);
            busy_validators.insert(*identity);
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to create validator stake accounts".into())
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn distribute_validator_stake<V>(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    stake_pool_address: &Pubkey,
    stake_pool: &StakePool,
    validator_list: &ValidatorList,
    desired_validator_stake: V,
    baseline_stake_amount: u64,
    bonus_stake_amount: u64,
) -> Result<bool, Box<dyn error::Error>>
where
    V: IntoIterator<Item = ValidatorStake>,
{
    let mut reserve_stake_balance = get_available_stake_balance(
        rpc_client,
        stake_pool.reserve_stake,
        MIN_STAKE_RESERVE_BALANCE,
    )
    .map_err(|err| {
        format!(
            "Unable to get reserve stake account balance: {}: {}",
            stake_pool.reserve_stake, err
        )
    })?;

    info!(
        "Reserve stake available balance before updates: {}",
        Sol(reserve_stake_balance)
    );

    // Prioritize funding smaller stake accounts to maximize the number of accounts that will be
    // funded with the available reserve stake.
    let mut min_stake = vec![];
    let mut baseline_stake = vec![];
    let mut bonus_stake = vec![];

    for validator_stake in desired_validator_stake {
        match validator_list.find(&validator_stake.vote_address) {
            None => warn!(
                "Vote address {} found in desired validator stake, but not in stake pool",
                &validator_stake.vote_address
            ),
            Some(validator_entry) => {
                let list = match validator_stake.stake_state {
                    ValidatorStakeState::None => &mut min_stake,
                    ValidatorStakeState::Baseline => &mut baseline_stake,
                    ValidatorStakeState::Bonus => &mut bonus_stake,
                };

                list.push((validator_entry.stake_lamports, validator_stake));
            }
        }
    }

    // Sort from lowest to highest balance
    min_stake.sort_by_key(|k| k.0);
    baseline_stake.sort_by_key(|k| k.0);
    bonus_stake.sort_by_key(|k| k.0);

    let mut transactions = vec![];
    for (
        balance,
        ValidatorStake {
            identity,
            stake_state,
            vote_address,
        },
    ) in min_stake
        .into_iter()
        .chain(baseline_stake)
        .chain(bonus_stake)
    {
        let desired_balance = match stake_state {
            ValidatorStakeState::None => 0,
            ValidatorStakeState::Baseline => baseline_stake_amount,
            ValidatorStakeState::Bonus => bonus_stake_amount,
        };
        info!(
            "desired stake for {} ({:?}) is {}, current balance is {}",
            identity,
            stake_state,
            Sol(desired_balance),
            Sol(balance)
        );

        #[allow(clippy::comparison_chain)]
        let op_msg = if balance > desired_balance {
            let amount_to_remove = balance - desired_balance;
            if amount_to_remove < MIN_STAKE_CHANGE_AMOUNT {
                format!("not removing {} (amount too small)", Sol(amount_to_remove))
            } else {
                transactions.push(Transaction::new_with_payer(
                    &[
                        spl_stake_pool::instruction::decrease_validator_stake_with_vote(
                            stake_pool,
                            stake_pool_address,
                            &vote_address,
                            amount_to_remove,
                        ),
                    ],
                    Some(&authorized_staker.pubkey()),
                ));
                format!("removing {}", Sol(amount_to_remove))
            }
        } else if balance < desired_balance {
            let mut amount_to_add = desired_balance - balance;

            if amount_to_add < MIN_STAKE_CHANGE_AMOUNT {
                format!("not adding {} (amount too small)", Sol(amount_to_add))
            } else {
                if amount_to_add > reserve_stake_balance {
                    trace!(
                        "note: amount_to_add > reserve_stake_balance: {} > {}",
                        amount_to_add,
                        reserve_stake_balance
                    );
                    amount_to_add = reserve_stake_balance;
                }

                if amount_to_add < MIN_STAKE_CHANGE_AMOUNT {
                    "reserve depleted".to_string()
                } else {
                    reserve_stake_balance -= amount_to_add;
                    info!("adding {} stake", Sol(amount_to_add));

                    transactions.push(Transaction::new_with_payer(
                        &[
                            spl_stake_pool::instruction::increase_validator_stake_with_vote(
                                stake_pool,
                                stake_pool_address,
                                &vote_address,
                                amount_to_add,
                            ),
                        ],
                        Some(&authorized_staker.pubkey()),
                    ));
                    format!("adding {}", Sol(amount_to_add))
                }
            }
        } else {
            "no change".to_string()
        };

        debug!(
            "{} ({:?}) target: {}, current: {}, {}",
            identity,
            stake_state,
            Sol(desired_balance),
            Sol(balance),
            op_msg,
        );
    }
    info!(
        "Reserve stake available balance after updates: {}",
        Sol(reserve_stake_balance)
    );

    let ok = send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty();

    if !ok {
        error!("One or more transactions failed to execute")
    }
    Ok(ok)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::rpc_client_utils::test::*,
        solana_sdk::{
            clock::Epoch,
            epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
            native_token::sol_to_lamports,
            signature::{Keypair, Signer},
        },
        solana_validator::test_validator::*,
        spl_stake_pool::find_withdraw_authority_program_address,
    };

    fn num_stake_accounts(rpc_client: &RpcClient, authority: Pubkey) -> usize {
        get_all_stake(rpc_client, authority).unwrap().0.len()
    }

    fn validator_stake_balance(
        rpc_client: &RpcClient,
        stake_pool_address: &Pubkey,
        validator: &ValidatorAddressPair,
    ) -> u64 {
        let stake_rent_exemption =
            get_minimum_stake_balance_for_rent_exemption(rpc_client).unwrap();
        let min_stake_account_balance = stake_rent_exemption + MIN_STAKE_ACCOUNT_BALANCE;
        let stake_address = find_stake_program_address(
            &spl_stake_pool::id(),
            &validator.vote_address,
            stake_pool_address,
        )
        .0;
        let stake_balance = rpc_client.get_balance(&stake_address).unwrap();
        info!("Stake {} has balance {}", stake_address, stake_balance);
        stake_balance - min_stake_account_balance
    }

    fn uniform_stake_pool_apply(
        stake_o_matic: &mut StakePoolOMatic,
        rpc_client: &RpcClient,
        validators: &[ValidatorAddressPair],
        stake_state: ValidatorStakeState,
        expected_validator_stake_balance: u64,
        expected_reserve_stake_balance: u64,
    ) {
        let pool_withdraw_authority = find_withdraw_authority_program_address(
            &spl_stake_pool::id(),
            &stake_o_matic.stake_pool_address,
        )
        .0;

        let desired_validator_stake = validators
            .iter()
            .map(|vap| ValidatorStake {
                identity: vap.identity,
                vote_address: vap.vote_address,
                stake_state,
            })
            .collect::<Vec<_>>();

        stake_o_matic
            .apply(rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert!(num_stake_accounts(rpc_client, pool_withdraw_authority) > 1 + validators.len());
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        stake_o_matic
            .apply(rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert_eq!(
            num_stake_accounts(rpc_client, pool_withdraw_authority),
            1 + validators.len()
        );
        assert_eq!(
            rpc_client
                .get_balance(&stake_o_matic.stake_pool.reserve_stake)
                .unwrap(),
            expected_reserve_stake_balance
        );
        for validator in validators {
            assert_eq!(
                validator_stake_balance(rpc_client, &stake_o_matic.stake_pool_address, validator),
                expected_validator_stake_balance
            );
        }
    }

    #[test]
    fn this_test_is_too_big_and_slow() {
        solana_logger::setup_with_default("solana_stake_o_matic=info");

        let mut test_validator_genesis = TestValidatorGenesis::default();
        test_validator_genesis
            .epoch_schedule(EpochSchedule::custom(
                MINIMUM_SLOTS_PER_EPOCH,
                MINIMUM_SLOTS_PER_EPOCH,
                /* enable_warmup_epochs = */ false,
            ))
            .add_program("spl_stake_pool", spl_stake_pool::id());
        let (test_validator, authorized_staker) = test_validator_genesis.start();

        let (rpc_client, _recent_blockhash, _fee_calculator) = test_validator.rpc_client();

        let stake_pool = Keypair::new();
        let pool_withdraw_authority =
            find_withdraw_authority_program_address(&spl_stake_pool::id(), &stake_pool.pubkey()).0;

        let assert_validator_stake_activation =
            |vap: &ValidatorAddressPair, epoch: Epoch, state: StakeActivationState| {
                let stake_address = find_stake_program_address(
                    &spl_stake_pool::id(),
                    &vap.vote_address,
                    &stake_pool.pubkey(),
                )
                .0;
                assert_eq!(
                    rpc_client
                        .get_stake_activation(stake_address, Some(epoch))
                        .unwrap()
                        .state,
                    state,
                );
            };

        // ===========================================================
        info!("Create stake pool: mint, fee account, reserve stake, and pool itself");
        let stake_rent_exemption =
            get_minimum_stake_balance_for_rent_exemption(&rpc_client).unwrap();
        let withdraw_authority =
            find_withdraw_authority_program_address(&spl_stake_pool::id(), &stake_pool.pubkey()).0;
        let pool_mint = create_mint(&rpc_client, &authorized_staker, &withdraw_authority).unwrap();
        let pool_fee_account = create_token_account(
            &rpc_client,
            &authorized_staker,
            &pool_mint,
            &authorized_staker.pubkey(),
        )
        .unwrap();
        let pool_reserve_stake = create_stake_account(
            &rpc_client,
            &authorized_staker,
            &withdraw_authority,
            stake_rent_exemption + MIN_STAKE_RESERVE_BALANCE,
        )
        .unwrap()
        .pubkey();
        let num_validators = 3;
        create_stake_pool(
            &rpc_client,
            &authorized_staker,
            &stake_pool,
            &pool_reserve_stake,
            &pool_mint,
            &pool_fee_account,
            &authorized_staker,
            &authorized_staker.pubkey(),
            num_validators,
        )
        .unwrap();

        info!("Create three validators");
        let validators =
            create_validators(&rpc_client, &authorized_staker, num_validators).unwrap();

        let baseline_stake_amount = sol_to_lamports(10.);
        let total_stake_amount =
            (baseline_stake_amount + sol_to_lamports(100.)) * validators.len() as u64;
        let total_stake_amount_plus_min =
            total_stake_amount + stake_rent_exemption + MIN_STAKE_RESERVE_BALANCE;

        let assert_reserve_account_only = |current_reserve_amount| {
            assert_eq!(
                rpc_client.get_balance(&pool_reserve_stake).unwrap(),
                current_reserve_amount,
            );
            {
                let (all_stake, all_stake_total_amount) =
                    get_all_stake(&rpc_client, withdraw_authority).unwrap();
                assert_eq!(all_stake_total_amount, current_reserve_amount);
                assert_eq!(all_stake.len(), 1);
                assert!(all_stake.contains(&pool_reserve_stake));
            }
        };
        assert_reserve_account_only(MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption);

        let mut stake_o_matic = new(
            &rpc_client,
            authorized_staker,
            stake_pool.pubkey(),
            baseline_stake_amount,
        )
        .unwrap();

        // ===========================================================
        info!(
            "Start with creating validator stake accounts and deposit stake, no managed stake yet"
        );
        let epoch = rpc_client.get_epoch_info().unwrap().epoch;
        stake_o_matic
            .apply(
                &rpc_client,
                false,
                &validators
                    .iter()
                    .map(|vap| ValidatorStake {
                        identity: vap.identity,
                        vote_address: vap.vote_address,
                        stake_state: ValidatorStakeState::None,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let deposit_stake_address = create_stake_account(
            &rpc_client,
            &stake_o_matic.authorized_staker,
            &stake_o_matic.authorized_staker.pubkey(),
            total_stake_amount,
        )
        .unwrap()
        .pubkey();
        let deposit_vote_address = validators.first().unwrap().vote_address;
        delegate_stake(
            &rpc_client,
            &stake_o_matic.authorized_staker,
            &deposit_stake_address,
            &deposit_vote_address,
        )
        .unwrap();

        info!("min: wait for stake activation");
        assert_eq!(
            rpc_client.get_balance(&pool_reserve_stake).unwrap(),
            MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption,
        );

        for validator in &validators {
            assert_validator_stake_activation(validator, epoch, StakeActivationState::Activating);
            assert_eq!(
                validator_stake_balance(&rpc_client, &stake_pool.pubkey(), validator),
                0,
            );
        }
        assert_eq!(num_stake_accounts(&rpc_client, pool_withdraw_authority), 1);
        assert_eq!(
            num_stake_accounts(&rpc_client, stake_o_matic.authorized_staker.pubkey()),
            validators.len() + 1
        );
        let epoch = wait_for_next_epoch(&rpc_client).unwrap();

        // To simulate a reward-earning environment, we add a few lamports
        // to the validator stake accounts. This way, during the
        // `add_validators_to_pool` phase, we can test the logic to split from
        // the validator stake account.
        for validator in &validators {
            assert_validator_stake_activation(validator, epoch, StakeActivationState::Active);
            let stake_address = find_stake_program_address(
                &spl_stake_pool::id(),
                &validator.vote_address,
                &stake_o_matic.stake_pool_address,
            )
            .0;
            transfer(
                &rpc_client,
                &stake_o_matic.authorized_staker,
                &stake_address,
                30,
            )
            .unwrap();
        }

        // ===========================================================
        stake_o_matic.epoch_update(&rpc_client).unwrap();
        info!("Add all validators to the pool");
        stake_o_matic
            .apply(
                &rpc_client,
                false,
                &validators
                    .iter()
                    .map(|vap| ValidatorStake {
                        identity: vap.identity,
                        vote_address: vap.vote_address,
                        stake_state: ValidatorStakeState::None,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        info!("Deposit stake");
        let staker_pool_token_address = create_token_account(
            &rpc_client,
            &stake_o_matic.authorized_staker,
            &pool_mint,
            &stake_o_matic.authorized_staker.pubkey(),
        )
        .unwrap();
        deposit_into_stake_pool(
            &rpc_client,
            &stake_o_matic.authorized_staker,
            &stake_o_matic.stake_pool_address,
            &stake_o_matic.stake_pool,
            &deposit_vote_address,
            &deposit_stake_address,
            &staker_pool_token_address,
        )
        .unwrap();

        info!("All validators to nothing, moving all to reserve");
        stake_o_matic
            .apply(
                &rpc_client,
                false,
                &validators
                    .iter()
                    .map(|vap| ValidatorStake {
                        identity: vap.identity,
                        vote_address: vap.vote_address,
                        stake_state: ValidatorStakeState::None,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();

        // ===========================================================
        stake_o_matic.epoch_update(&rpc_client).unwrap();
        info!("All validators to baseline");
        uniform_stake_pool_apply(
            &mut stake_o_matic,
            &rpc_client,
            &validators,
            ValidatorStakeState::Baseline,
            baseline_stake_amount,
            total_stake_amount_plus_min - baseline_stake_amount * validators.len() as u64,
        );

        // ===========================================================
        info!("All the validators to bonus stake level");
        uniform_stake_pool_apply(
            &mut stake_o_matic,
            &rpc_client,
            &validators,
            ValidatorStakeState::Bonus,
            total_stake_amount / validators.len() as u64,
            MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption,
        );

        // ===========================================================
        info!("Different stake for each validator");
        let desired_validator_stake = vec![
            ValidatorStake {
                identity: validators[0].identity,
                vote_address: validators[0].vote_address,
                stake_state: ValidatorStakeState::None,
            },
            ValidatorStake {
                identity: validators[1].identity,
                vote_address: validators[1].vote_address,
                stake_state: ValidatorStakeState::Baseline,
            },
            ValidatorStake {
                identity: validators[2].identity,
                vote_address: validators[2].vote_address,
                stake_state: ValidatorStakeState::Bonus,
            },
        ];

        stake_o_matic
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        stake_o_matic
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();

        info!("Check after first epoch");
        // after the first epoch, validators 0 and 1 are at their target levels but validator 2
        // needs one more epoch for the additional bonus stake to arrive
        for (validator, expected_sol_balance) in validators.iter().zip(&[0., 10., 110.]) {
            assert_eq!(
                sol_to_lamports(*expected_sol_balance),
                validator_stake_balance(&rpc_client, &stake_pool.pubkey(), validator),
                "stake balance mismatch for validator {}, expected {}",
                validator.identity,
                expected_sol_balance
            );
        }

        assert_eq!(
            rpc_client
                .get_balance(&stake_o_matic.stake_pool.reserve_stake)
                .unwrap(),
            MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption,
        );

        info!("Check after second epoch");
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        stake_o_matic
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert_eq!(
            rpc_client
                .get_balance(&stake_o_matic.stake_pool.reserve_stake)
                .unwrap(),
            MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption,
        );

        // after the second epoch, validator 2 is now has all the bonus stake
        for (validator, expected_sol_balance) in validators.iter().zip(&[0., 10., 320.]) {
            assert_eq!(
                sol_to_lamports(*expected_sol_balance),
                validator_stake_balance(&rpc_client, &stake_pool.pubkey(), validator),
                "stake balance mismatch for validator {}",
                validator.identity
            );
        }

        // ===========================================================
        info!("remove all validators");

        // deactivate all validator stake and remove from pool
        stake_o_matic.apply(&rpc_client, false, &[]).unwrap();
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        // withdraw removed validator stake into the staker
        stake_o_matic.apply(&rpc_client, false, &[]).unwrap();
        // all stake has been returned to the reserve account
        assert_reserve_account_only(
            MIN_STAKE_RESERVE_BALANCE + stake_rent_exemption + total_stake_amount,
        );
        // staker has recovered all of their SOL from stake accounts
        assert_eq!(
            num_stake_accounts(&rpc_client, stake_o_matic.authorized_staker.pubkey()),
            0
        );
    }
}
