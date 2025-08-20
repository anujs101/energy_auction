use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, TokenAccount, Token};

declare_id!("5V4D1b9wrjuJC3aAtNbayVgMYt5879w2rL2k5UoQGTvM");

#[program]
pub mod energy_auction {
    use super::*;

    /// Initialize the global protocol state
    pub fn init_global_state(
        ctx: Context<InitGlobalState>,
        fee_bps: u16,
        version: u8,
    ) -> Result<()> {
        let state = &mut ctx.accounts.global_state;
        state.authority = ctx.accounts.authority.key();
        state.fee_bps = fee_bps;
        state.version = version;
        state.quote_mint = ctx.accounts.quote_mint.key();
        state.fee_vault = ctx.accounts.fee_vault.key();
        Ok(())
    }

    /// Seller commits supply (one-time per (global_state, timeslot, seller))
    pub fn commit_supply(ctx: Context<CommitSupply>, timeslot: u64, amount: u64) -> Result<()> {
        let supply = &mut ctx.accounts.supply;

        // initialize supply account fields (immutable after init)
        supply.supplier = ctx.accounts.signer.key();
        supply.timeslot = timeslot;
        supply.amount = amount;
        supply.bump = ctx.bumps.supply;

        emit!(SupplyCommitted {
            supplier: supply.supplier,
            timeslot,
            amount,
        });

        Ok(())
    }

    // TODO: add open_timeslot, place_bid, settle, redeem...
}

///////////////////////
// Contexts
///////////////////////

#[derive(Accounts)]
pub struct InitGlobalState<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + GlobalState::LEN,
        seeds = [b"global_state"],
        bump
    )]
    pub global_state: Account<'info, GlobalState>,

    pub quote_mint: Account<'info, Mint>, // USDC or other quote token

    #[account(
        init,
        payer = authority,
        token::mint = quote_mint,
        token::authority = global_state,
        seeds = [b"fee_vault"],
        bump
    )]
    pub fee_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    // rent removed (not used in MVP)
}

/// Seller commits supply for a specific timeslot (one-time)
#[derive(Accounts)]
#[instruction(timeslot: u64)]
pub struct CommitSupply<'info> {
    /// Global protocol state (read/write allowed)
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,

    /// Supply account for this (global_state, timeslot, signer).
    /// init-only: cannot be recreated if exists.
    #[account(
        init,
        payer = signer,
        space = 8 + Supply::LEN,
        seeds = [b"commit", global_state.key().as_ref(), &timeslot.to_le_bytes(), signer.key().as_ref()],
        bump
    )]
    pub supply: Account<'info, Supply>,

    /// Seller committing energy (payer & signer)
    #[account(mut)]
    pub signer: Signer<'info>,

    /// Programs
    pub system_program: Program<'info, System>,
}

///////////////////////
// Events
///////////////////////

#[event]
pub struct SupplyCommitted {
    pub supplier: Pubkey,
    pub timeslot: u64,
    pub amount: u64,
}

///////////////////////
// State
///////////////////////

/// Global protocol config
#[account]
pub struct GlobalState {
    pub authority: Pubkey,   // protocol admin
    pub fee_bps: u16,        // protocol fee in basis points
    pub version: u8,         // versioning for upgrades
    pub quote_mint: Pubkey,  // e.g., USDC
    pub fee_vault: Pubkey,   // PDA token account for protocol fees
}

impl GlobalState {
    pub const LEN: usize = 32  // authority
        + 2                    // fee_bps
        + 1                    // version
        + 32                   // quote_mint
        + 32;                  // fee_vault
}

/// Minimal Supply struct for MVP (one-time immutable per timeslot)
#[account]
pub struct Supply {
    pub supplier: Pubkey,     // Who committed
    pub timeslot: u64,        // Timeslot for which supply is committed
    pub amount: u64,          // Amount committed (lots)
    pub bump: u8,             // PDA bump
}

impl Supply {
    pub const LEN: usize = 32 + 8 + 8 + 1;
}

/// Auction round container (left as skeleton for now)
#[account]
pub struct Timeslot {
    pub epoch_ts: i64,        // identifies auction window
    pub status: u8,           // Pending=0, Open=1, Sealed=2, Settled=3, Cancelled=4
    pub lot_size: u64,        // fixed per auction (1 kWh MVP)
    pub quote_mint: Pubkey,   // quote token (USDC)
    pub price_tick: u64,      // min price increment
    pub total_supply: u64,    // total committed lots
    pub total_bids: u64,      // total lots bid
    pub head_page: Option<Pubkey>, // linked list of BidPages
    pub tail_page: Option<Pubkey>, // last BidPage
}

impl Timeslot {
    pub const LEN: usize = 8   // epoch_ts
        + 1                   // status
        + 8                   // lot_size
        + 32                  // quote_mint
        + 8                   // price_tick
        + 8                   // total_supply
        + 8                   // total_bids
        + 1 + 32              // head_page (Option<Pubkey>)
        + 1 + 32;             // tail_page (Option<Pubkey>)
}

/// A single bid entry
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct Bid {
    pub owner: Pubkey,
    pub price: u64,
    pub quantity: u64,
    pub timestamp: i64,
    pub status: u8, // Active=0, Cancelled=1, Filled=2
}

impl Bid {
    pub const LEN: usize = 32  // owner
        + 8                    // price
        + 8                    // quantity
        + 8                    // timestamp
        + 1;                   // status
}

/// Page of bids (linked list)
#[account]
pub struct BidPage {
    pub timeslot: Pubkey,         // which timeslot
    pub bids: Vec<Bid>,           // fixed max length (MVP: 200)
    pub next_page: Option<Pubkey>,
}

impl BidPage {
    pub const MAX_BIDS: usize = 200;
    pub const LEN: usize = 32                  // timeslot
        + 4 + (Bid::LEN * Self::MAX_BIDS)     // Vec<Bid>
        + 1 + 32;                             // next_page
}

/// Receipt created for each winning buyer after settlement
#[account]
pub struct FillReceipt {
    pub buyer: Pubkey,
    pub timeslot: Pubkey,
    pub quantity: u64,
    pub clearing_price: u64,
    pub redeemed: bool,
}

impl FillReceipt {
    pub const LEN: usize = 32  // buyer
        + 32                   // timeslot
        + 8                    // quantity
        + 8                    // clearing_price
        + 1;                   // redeemed
}

/// Protocol fee vault (separate from sellers’ escrows)
#[account]
pub struct FeeVault {
    pub bump: u8,              // PDA bump
    pub token_account: Pubkey, // SPL Token account PDA
}

impl FeeVault {
    pub const LEN: usize = 1 + 32;
}

#[error_code]
pub enum EnergyAuctionError {
    /// The provided authority does not match the global state authority
    #[msg("Invalid authority for this operation")]
    InvalidAuthority,

    /// Supply already exists for this seller and timeslot
    #[msg("Supply already committed for this seller and timeslot")]
    DuplicateSupply,

    /// Timeslot is not active or invalid
    #[msg("Timeslot is not active or has expired")]
    InvalidTimeslot,

    /// Seller does not have enough tokens to commit supply
    #[msg("Insufficient token balance to commit supply")]
    InsufficientBalance,

    /// Overflow or underflow during arithmetic
    #[msg("Math overflow/underflow error")]
    MathError,

    /// Provided escrow vault PDA does not match expected PDA
    #[msg("Invalid escrow vault account")]
    InvalidEscrowVault,

    /// Unauthorized signer tried to call this instruction
    #[msg("Unauthorized signer for this transaction")]
    Unauthorized,

    /// Global state account mismatch
    #[msg("Invalid global state account provided")]
    InvalidGlobalState,

    /// General constraint violation
    #[msg("Account constraint violated")]
    ConstraintViolation,
}
