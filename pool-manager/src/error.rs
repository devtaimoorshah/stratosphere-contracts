use crate::manager::commands::MAX_ASSETS_PER_POOL;
use cosmwasm_std::{
    CheckedFromRatioError, CheckedMultiplyFractionError, CheckedMultiplyRatioError,
    ConversionOverflowError, DivideByZeroError, Instantiate2AddressError, OverflowError, StdError,
    Uint128,
};
use cw_migrate_error_derive::cw_migrate_invalid_version_error;
use cw_ownable::OwnershipError;
use cw_utils::PaymentError;
use thiserror::Error;

#[cw_migrate_invalid_version_error]
#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    // Handle all normal errors from the StdError
    #[error("{0}")]
    Std(#[from] StdError),

    // Handle errors specific to payments from cw-util
    #[error("{0}")]
    PaymentError(#[from] PaymentError),

    #[error(transparent)]
    Instantiate2Error(#[from] Instantiate2AddressError),

    // Handle ownership errors from cw-ownable
    #[error("{0}")]
    OwnershipError(#[from] OwnershipError),

    // Handle Upgrade/Migrate related semver errors
    #[error("Semver parsing error: {0}")]
    SemVer(String),

    #[error("Unauthorized")]
    Unauthorized,
    // Add any other custom errors you like here.
    // Look at https://docs.rs/thiserror/1.0.21/thiserror/ for details.
    #[error("The provided assets are both the same")]
    SameAsset,

    #[error(
        "Assertion failed; minimum receive amount: {minimum_receive}, swap amount: {swap_amount}"
    )]
    MinimumReceiveAssertion {
        minimum_receive: Uint128,
        swap_amount: Uint128,
    },

    #[error("The asset \"{asset_infos}\" with the identifier \"{identifier}\" already has a pool")]
    PoolExists {
        asset_infos: String, //String representation of the asset infos
        identifier: String,
    },

    #[error("More assets provided than it is supported. The max is currently {MAX_ASSETS_PER_POOL}, you provided {assets_provided}")]
    TooManyAssets { assets_provided: usize },

    #[error("{asset} is invalid")]
    InvalidAsset { asset: String },

    #[error("Trying to provide liquidity without any assets")]
    EmptyAssets,

    #[error("Invalid single side liquidity provision swap, expected {expected} got {actual}")]
    InvalidSingleSideLiquidityProvisionSwap { expected: Uint128, actual: Uint128 },

    #[error("Cannot provide single-side liquidity when the pool is empty")]
    EmptyPoolForSingleSideLiquidityProvision,

    #[error("Cannot provide single-side liquidity on a pool with more than 2 assets")]
    InvalidPoolAssetsForSingleSideLiquidityProvision,

    #[error("Pool does not exist")]
    UnExistingPool,

    #[error("Operation disabled, {0}")]
    OperationDisabled(String),

    #[error("Initial liquidity amount must be over {0}")]
    InvalidInitialLiquidityAmount(Uint128),

    #[error("Failed to compute the LP share with the given deposit")]
    LiquidityShareComputationFailed,

    #[error("The amount of LP shares to withdraw is invalid")]
    InvalidLpShareToWithdraw,

    #[error("Spread limit exceeded")]
    MaxSpreadAssertion,

    #[error("Slippage tolerance exceeded")]
    MaxSlippageAssertion,

    #[error("The asset doesn't match the assets stored in contract")]
    AssetMismatch,

    #[error("Error computing the LP mint amount for the stable pool")]
    StableLpMintError,

    #[error("Error computing the stableswap invariant")]
    StableInvariantError,

    #[error("Failed to converge when performing newtons method")]
    ConvergeError,

    #[error("An conversion overflow occurred when attempting to swap an asset")]
    SwapOverflowError,

    #[error("An overflow occurred when attempting to construct a decimal")]
    DecimalOverflow,

    #[error("{0}")]
    OverflowError(#[from] OverflowError),

    #[error(transparent)]
    CheckedMultiplyRatioError(#[from] CheckedMultiplyRatioError),

    #[error(transparent)]
    CheckedMultiplyFractionError(#[from] CheckedMultiplyFractionError),

    #[error(transparent)]
    CheckedFromRatioError(#[from] CheckedFromRatioError),

    #[error(transparent)]
    DivideByZeroError(#[from] DivideByZeroError),

    #[error(transparent)]
    ConversionOverflowError(#[from] ConversionOverflowError),

    #[error("Must provide swap operations to execute")]
    NoSwapOperationsProvided,

    #[error("Attempt to perform non-consecutive swap operation from previous output of {previous_output} to next input of {next_input}")]
    NonConsecutiveSwapOperations {
        previous_output: String,
        next_input: String,
    },

    #[error("Invalid pool creation fee, expected {expected} got {amount}")]
    InvalidPoolCreationFee { amount: Uint128, expected: Uint128 },

    #[error("Pool creation fee was not included")]
    PoolCreationFeeMissing,

    #[error("Additional funds were sent with pool creation, expected pool creation and token factory fees only")]
    ExtraFundsSent,

    #[error("Funds for {denom} were missing when performing swap")]
    MissingNativeSwapFunds { denom: String },

    #[error("Invalid pool assets length, expected {expected} got {actual}")]
    InvalidPoolAssetsLength { expected: usize, actual: usize },

    #[error("The pool has no assets")]
    PoolHasNoAssets,

    #[error("Invalid LP asset {lp_asset}. This probably happened because the length of the subdenom was too long. Try to shorten the pool identifier.")]
    InvalidLpAsset { lp_asset: String },

    #[error("Invalid pool identifier {identifier}. Either too long or malformed, only alphanumeric characters, . and / are allowed.")]
    InvalidPoolIdentifier { identifier: String },

    #[error("The token factory lp denom creation fee was not paid.")]
    TokenFactoryFeeNotPaid,
}

impl From<semver::Error> for ContractError {
    fn from(err: semver::Error) -> Self {
        Self::SemVer(err.to_string())
    }
}
