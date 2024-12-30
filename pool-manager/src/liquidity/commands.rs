use cosmwasm_std::{
    coin, coins, ensure, to_json_binary, wasm_execute, BankMsg, Coin, CosmosMsg, Decimal256,
    DepsMut, Env, MessageInfo, Response, StdResult, SubMsg, Uint256,
};
use cosmwasm_std::{Decimal, Uint128};

use mantra_dex_std::coin::{add_coins, aggregate_coins};
use mantra_dex_std::common::validate_addr_or_default;
use mantra_dex_std::farm_manager::{PositionsBy, PositionsResponse};
use mantra_dex_std::lp_common::MINIMUM_LIQUIDITY_AMOUNT;
use mantra_dex_std::pool_manager::{get_total_share, ExecuteMsg, PoolType};
use mantra_dex_std::U256;

use crate::{
    helpers::{self},
    state::get_pool_by_identifier,
};
use crate::{
    state::{CONFIG, POOLS},
    ContractError,
};
// After writing create_pool I see this can get quite verbose so attempting to
// break it down into smaller modules which house some things like swap, liquidity etc
use crate::contract::SINGLE_SIDE_LIQUIDITY_PROVISION_REPLY_ID;
use crate::helpers::{
    aggregate_outgoing_fees, compute_d, compute_lp_mint_amount_for_stableswap_deposit,
};
use crate::queries::query_simulation;
use crate::state::{
    LiquidityProvisionData, SingleSideLiquidityProvisionBuffer,
    SINGLE_SIDE_LIQUIDITY_PROVISION_BUFFER,
};

#[allow(clippy::too_many_arguments)]
pub fn provide_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    slippage_tolerance: Option<Decimal>,
    max_spread: Option<Decimal>,
    receiver: Option<String>,
    pool_identifier: String,
    unlocking_duration: Option<u64>,
    lock_position_identifier: Option<String>,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    // check if the deposit feature is enabled
    ensure!(
        config.feature_toggle.deposits_enabled,
        ContractError::OperationDisabled("provide_liquidity".to_string())
    );

    // Get the pool by the pool_identifier
    let mut pool = get_pool_by_identifier(&deps.as_ref(), &pool_identifier)?;

    let mut pool_assets = pool.assets.clone();
    let deposits = aggregate_coins(info.funds.clone())?;

    ensure!(!deposits.is_empty(), ContractError::EmptyAssets);

    // verify that the assets sent match the ones from the pool
    ensure!(
        deposits.iter().all(|asset| pool_assets
            .iter()
            .any(|pool_asset| pool_asset.denom == asset.denom)),
        ContractError::AssetMismatch
    );

    let receiver =
        validate_addr_or_default(&deps.as_ref(), receiver, info.sender.clone()).to_string();

    //let receiver = receiver.unwrap_or_else(|| info.sender.to_string());
    // check if the user is providing liquidity with a single asset
    let is_single_asset_provision = deposits.len() == 1usize;

    if is_single_asset_provision {
        ensure!(
            !pool_assets.iter().any(|asset| asset.amount.is_zero()),
            ContractError::EmptyPoolForSingleSideLiquidityProvision
        );

        // can't provide single side liquidity on a pool with more than 2 assets
        ensure!(
            pool_assets.len() == 2,
            ContractError::InvalidPoolAssetsForSingleSideLiquidityProvision
        );

        let deposit = deposits[0].clone();

        let ask_asset_denom = pool_assets
            .iter()
            .find(|pool_asset| pool_asset.denom != deposit.denom)
            .ok_or(ContractError::AssetMismatch)?
            .denom
            .clone();

        // swap half of the deposit asset for the other asset in the pool
        let swap_half = Coin {
            denom: deposit.denom.clone(),
            amount: deposit.amount.checked_div_floor((2u64, 1u64))?,
        };

        let swap_simulation_response = query_simulation(
            deps.as_ref(),
            swap_half.clone(),
            ask_asset_denom.clone(),
            pool_identifier.clone(),
        )?;

        // let's compute the expected offer asset balance in the contract after the swap and liquidity
        // provision takes place. This should be the same value as of now. Even though half of it
        // will be swapped, eventually all of it will be sent to the contract in the second step of
        // the single side liquidity provision
        let expected_offer_asset_balance_in_contract = deps
            .querier
            .query_balance(&env.contract.address, deposit.denom)?;

        // let's compute the expected ask asset balance in the contract after the swap and liquidity
        // provision takes place. It should be the current balance minus the fees that will be sent
        // off the contract.
        let mut expected_ask_asset_balance_in_contract = deps
            .querier
            .query_balance(&env.contract.address, ask_asset_denom.clone())?;

        expected_ask_asset_balance_in_contract.amount = expected_ask_asset_balance_in_contract
            .amount
            .saturating_sub(aggregate_outgoing_fees(&swap_simulation_response)?);

        // sanity check. Theoretically, with the given conditions of min LP, pool fees and max spread assertion,
        // the expected ask asset balance in the contract will always be greater than zero after
        // subtracting the fees.
        ensure!(
            !expected_ask_asset_balance_in_contract.amount.is_zero(),
            ContractError::MaxSpreadAssertion
        );

        SINGLE_SIDE_LIQUIDITY_PROVISION_BUFFER.save(
            deps.storage,
            &SingleSideLiquidityProvisionBuffer {
                receiver,
                expected_offer_asset_balance_in_contract,
                expected_ask_asset_balance_in_contract,
                offer_asset_half: swap_half.clone(),
                expected_ask_asset: coin(
                    swap_simulation_response.return_amount.u128(),
                    ask_asset_denom.clone(),
                ),
                liquidity_provision_data: LiquidityProvisionData {
                    max_spread,
                    slippage_tolerance,
                    pool_identifier: pool_identifier.clone(),
                    unlocking_duration,
                    lock_position_identifier,
                },
            },
        )?;

        Ok(Response::default()
            .add_submessage(SubMsg::reply_on_success(
                wasm_execute(
                    env.contract.address.into_string(),
                    &ExecuteMsg::Swap {
                        ask_asset_denom,
                        belief_price: None,
                        max_spread,
                        receiver: None,
                        pool_identifier,
                    },
                    vec![swap_half],
                )?,
                SINGLE_SIDE_LIQUIDITY_PROVISION_REPLY_ID,
            ))
            .add_attributes(vec![("action", "single_side_liquidity_provision")]))
    } else {
        let mut messages: Vec<CosmosMsg> = vec![];

        let liquidity_token = pool.lp_denom.clone();

        // Compute share and other logic based on the number of assets
        let total_share = get_total_share(&deps.as_ref(), liquidity_token.clone())?;

        let share = match &pool.pool_type {
            PoolType::ConstantProduct => {
                if total_share == Uint128::zero() {
                    // Make sure at least MINIMUM_LIQUIDITY_AMOUNT is deposited to mitigate the risk of the first
                    // depositor preventing small liquidity providers from joining the pool
                    let share = Uint128::new(
                        (U256::from(deposits[0].amount.u128())
                            .checked_mul(U256::from(deposits[1].amount.u128()))
                            .ok_or::<ContractError>(
                                ContractError::LiquidityShareComputationFailed,
                            ))?
                        .integer_sqrt()
                        .as_u128(),
                    )
                    .saturating_sub(MINIMUM_LIQUIDITY_AMOUNT);

                    // share should be above zero after subtracting the MINIMUM_LIQUIDITY_AMOUNT
                    if share.is_zero() {
                        return Err(ContractError::InvalidInitialLiquidityAmount(
                            MINIMUM_LIQUIDITY_AMOUNT,
                        ));
                    }

                    messages.push(mantra_dex_std::lp_common::mint_lp_token_msg(
                        liquidity_token.clone(),
                        &env.contract.address,
                        &env.contract.address,
                        MINIMUM_LIQUIDITY_AMOUNT,
                    )?);

                    share
                } else {
                    let mut asset_shares = vec![];

                    for deposit in deposits.iter() {
                        let asset_denom = &deposit.denom;
                        let pool_asset_index = pool_assets
                            .iter()
                            .position(|pool_asset| &pool_asset.denom == asset_denom)
                            .ok_or(ContractError::AssetMismatch)?;

                        asset_shares.push(
                            deposit
                                .amount
                                .multiply_ratio(total_share, pool_assets[pool_asset_index].amount),
                        );
                    }

                    std::cmp::min(asset_shares[0], asset_shares[1])
                }
            }
            PoolType::StableSwap { amp: amp_factor } => {
                if total_share == Uint128::zero() {
                    // Make sure at least MINIMUM_LIQUIDITY_AMOUNT is deposited to mitigate the risk of the first
                    // depositor preventing small liquidity providers from joining the pool
                    let share = Uint128::try_from(compute_d(amp_factor, &deposits).unwrap())?
                        .saturating_sub(MINIMUM_LIQUIDITY_AMOUNT);

                    // share should be above zero after subtracting the min_lp_token_amount
                    if share.is_zero() {
                        return Err(ContractError::InvalidInitialLiquidityAmount(
                            MINIMUM_LIQUIDITY_AMOUNT,
                        ));
                    }

                    // mint the lp tokens to the contract
                    messages.push(mantra_dex_std::lp_common::mint_lp_token_msg(
                        liquidity_token.clone(),
                        &env.contract.address,
                        &env.contract.address,
                        MINIMUM_LIQUIDITY_AMOUNT,
                    )?);

                    share
                } else {
                    compute_lp_mint_amount_for_stableswap_deposit(
                        amp_factor,
                        // pool_assets hold the balances before the deposit was made
                        &pool_assets,
                        // add the deposit to the pool_assets to calculate the new balances
                        &add_coins(pool_assets.clone(), deposits.clone())?,
                        total_share,
                    )?
                    .ok_or(ContractError::StableLpMintError)?
                }
            }
        };

        // assert slippage tolerance
        helpers::assert_slippage_tolerance(
            &slippage_tolerance,
            &deposits,
            &pool_assets,
            pool.pool_type.clone(),
            share,
            total_share,
        )?;

        // if the unlocking duration is set, lock the LP tokens in the farm manager
        if let Some(unlocking_duration) = unlocking_duration {
            // check if receiver is the same as the sender of the tx
            ensure!(
                receiver == info.sender.to_string(),
                ContractError::Unauthorized
            );

            // mint the lp tokens to the contract
            messages.push(mantra_dex_std::lp_common::mint_lp_token_msg(
                liquidity_token.clone(),
                &env.contract.address,
                &env.contract.address,
                share,
            )?);

            // if the lock_position_identifier is set
            if let Some(position_identifier) = lock_position_identifier {
                let positions_result: StdResult<PositionsResponse> = deps.querier.query_wasm_smart(
                    config.farm_manager_addr.to_string(),
                    &mantra_dex_std::farm_manager::QueryMsg::Positions {
                        filter_by: Some(PositionsBy::Identifier(position_identifier.clone())),
                        open_state: None,
                        start_after: None,
                        limit: None,
                    },
                );

                // a position with the given identifier exists
                if let Ok(positions_response) = positions_result {
                    // if the position exists, check if the receiver is the same as the sender
                    // if so, expand the position
                    ensure!(
                        positions_response.positions.len() == 1
                            && positions_response.positions[0].identifier == position_identifier
                            && positions_response.positions[0].receiver.to_string() == receiver,
                        ContractError::Unauthorized
                    );

                    messages.push(
                        wasm_execute(
                            config.farm_manager_addr,
                            &mantra_dex_std::farm_manager::ExecuteMsg::ManagePosition {
                                action: mantra_dex_std::farm_manager::PositionAction::Expand {
                                    identifier: position_identifier,
                                },
                            },
                            coins(share.u128(), liquidity_token),
                        )?
                        .into(),
                    );
                } else {
                    // a position with the given identifier does not exist, create a new position
                    // for the user
                    messages.push(
                        wasm_execute(
                            config.farm_manager_addr,
                            &mantra_dex_std::farm_manager::ExecuteMsg::ManagePosition {
                                action: mantra_dex_std::farm_manager::PositionAction::Create {
                                    identifier: Some(position_identifier),
                                    unlocking_duration,
                                    receiver: Some(receiver.clone()),
                                },
                            },
                            coins(share.u128(), liquidity_token),
                        )?
                        .into(),
                    );
                }
            } else {
                // no lock_position_identifier was set, create a new position for the user
                messages.push(
                    wasm_execute(
                        config.farm_manager_addr,
                        &mantra_dex_std::farm_manager::ExecuteMsg::ManagePosition {
                            action: mantra_dex_std::farm_manager::PositionAction::Create {
                                identifier: lock_position_identifier,
                                unlocking_duration,
                                receiver: Some(receiver.clone()),
                            },
                        },
                        coins(share.u128(), liquidity_token),
                    )?
                    .into(),
                );
            }
        } else {
            // if no unlocking duration is set, just mint the LP tokens to the receiver
            messages.push(mantra_dex_std::lp_common::mint_lp_token_msg(
                liquidity_token,
                &deps.api.addr_validate(&receiver)?,
                &env.contract.address,
                share,
            )?);
        }

        // Increment the pool asset amount by the amount sent
        for asset in deposits.iter() {
            let asset_denom = &asset.denom;
            let pool_asset_index = pool_assets
                .iter()
                .position(|pool_asset| &pool_asset.denom == asset_denom)
                .ok_or(ContractError::AssetMismatch)?;

            pool_assets[pool_asset_index].amount = pool_assets[pool_asset_index]
                .amount
                .checked_add(asset.amount)?;
        }

        pool.assets = pool_assets.clone();

        POOLS.save(deps.storage, &pool_identifier, &pool)?;

        Ok(Response::new().add_messages(messages).add_attributes(vec![
            ("action", "provide_liquidity"),
            ("sender", info.sender.as_str()),
            ("receiver", receiver.as_str()),
            (
                "assets",
                &pool_assets
                    .iter()
                    .map(|asset| asset.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            ("share", &share.to_string()),
        ]))
    }
}

/// Withdraws the liquidity. The user burns the LP tokens in exchange for the tokens provided, including
/// the swap fees accrued by its share of the pool.
pub fn withdraw_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    pool_identifier: String,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    // check if the withdraw feature is enabled
    if !config.feature_toggle.withdrawals_enabled {
        return Err(ContractError::OperationDisabled(
            "withdraw_liquidity".to_string(),
        ));
    }

    // Get the pool by the pool_identifier
    let mut pool = get_pool_by_identifier(&deps.as_ref(), &pool_identifier)?;
    let liquidity_token = pool.lp_denom.clone();
    // Verify that the LP token was sent
    let amount = cw_utils::must_pay(&info, &liquidity_token)?;

    // Get the total share of the pool
    let total_share = get_total_share(&deps.as_ref(), liquidity_token.clone())?;

    // Get the ratio of the amount to withdraw to the total share
    let share_ratio: Decimal256 = Decimal256::from_ratio(amount, total_share);

    // sanity check, the share_ratio cannot possibly be greater than 1
    ensure!(
        share_ratio <= Decimal256::one(),
        ContractError::InvalidLpShareToWithdraw
    );

    // Use the ratio to calculate the amount of each pool asset to refund
    let refund_assets: Vec<Coin> = pool
        .assets
        .iter()
        .map(|pool_asset| {
            Ok(Coin {
                denom: pool_asset.denom.clone(),
                amount: Uint128::try_from(
                    Decimal256::from_ratio(pool_asset.amount, Uint256::one())
                        .checked_mul(share_ratio)?
                        .to_uint_floor(),
                )?,
            })
        })
        .collect::<Result<Vec<Coin>, ContractError>>()?
        .into_iter()
        // filter out assets with zero amount
        .filter(|coin| coin.amount > Uint128::zero())
        .collect();

    let mut messages: Vec<CosmosMsg> = vec![];

    // Transfer the refund assets to the sender
    messages.push(CosmosMsg::Bank(BankMsg::Send {
        to_address: info.sender.to_string(),
        amount: refund_assets.clone(),
    }));

    // Deduct balances on pool_info by the amount of each refund asset
    for refund_asset in refund_assets.iter() {
        let refund_asset_denom = &refund_asset.denom;
        let pool_asset_index = pool
            .assets
            .iter()
            .position(|pool_asset| &pool_asset.denom == refund_asset_denom)
            .ok_or(ContractError::AssetMismatch)?;

        pool.assets[pool_asset_index].amount = pool.assets[pool_asset_index]
            .amount
            .checked_sub(refund_asset.amount)?;
    }

    POOLS.save(deps.storage, &pool_identifier, &pool)?;

    // Burn the LP tokens
    messages.push(mantra_dex_std::lp_common::burn_lp_asset_msg(
        liquidity_token,
        env.contract.address,
        amount,
    )?);
    // update pool info
    Ok(Response::new()
        .add_messages(messages)
        .set_data(to_json_binary(&refund_assets)?)
        .add_attributes(vec![
            ("action", "withdraw_liquidity"),
            ("sender", info.sender.as_str()),
            ("withdrawn_share", &amount.to_string()),
        ]))
}
