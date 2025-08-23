use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

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
        require!(fee_bps <= 10_000, EnergyAuctionError::ConstraintViolation);

        let state = &mut ctx.accounts.global_state;
        state.authority = ctx.accounts.authority.key();
        state.fee_bps = fee_bps;
        state.version = version;
        state.quote_mint = ctx.accounts.quote_mint.key();
        state.fee_vault = ctx.accounts.fee_vault.key();

        Ok(())
    }

    /// Open a new auction timeslot
    pub fn open_timeslot(
        ctx: Context<OpenTimeslot>,
        epoch_ts: i64,
        lot_size: u64,
        price_tick: u64,
    ) -> Result<()> {
        // only protocol authority may open
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        require!(lot_size > 0, EnergyAuctionError::ConstraintViolation);
        require!(price_tick > 0, EnergyAuctionError::ConstraintViolation);

        let slot = &mut ctx.accounts.timeslot;
        slot.epoch_ts = epoch_ts;
        slot.status = TimeslotStatus::Open as u8; // Open
        slot.lot_size = lot_size;
        slot.quote_mint = ctx.accounts.global_state.quote_mint;
        slot.price_tick = price_tick;
        slot.total_supply = 0;
        slot.total_bids = 0;
        slot.head_page = None;
        slot.tail_page = None;
        slot.clearing_price = 0;
        slot.total_sold_quantity = 0; // Initialize new field
        Ok(())
    }

    /// Seller commits supply (one-time per (global_state, timeslot, seller))
    /// Escrows seller's energy tokens into a program-owned vault (authority = timeslot PDA)
    pub fn commit_supply(
        ctx: Context<CommitSupply>,
        timeslot_epoch: i64,
        reserve_price: u64,
        quantity: u64,
    ) -> Result<()> {
        require!(quantity > 0, EnergyAuctionError::ConstraintViolation);
        let ts = &mut ctx.accounts.timeslot;
        require!(matches!(ts.status(), TimeslotStatus::Open), EnergyAuctionError::InvalidTimeslot);

        let supply = &mut ctx.accounts.supply;
        supply.supplier      = ctx.accounts.signer.key();
        supply.timeslot      = ts.key();
        supply.amount        = quantity;
        supply.reserve_price = reserve_price;
        supply.bump          = ctx.bumps.supply;
        supply.energy_mint   = ctx.accounts.energy_mint.key();
        supply.escrow_vault  = ctx.accounts.seller_escrow.key();
        supply.claimed       = false;

        // move energy tokens: seller_source -> seller_escrow (authority = signer)
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.seller_source.to_account_info(),
                to: ctx.accounts.seller_escrow.to_account_info(),
                authority: ctx.accounts.signer.to_account_info(),
            },
        );
        token::transfer(cpi_ctx, quantity)?;

        ts.total_supply = ts.total_supply.checked_add(quantity).ok_or(EnergyAuctionError::MathError)?;

        emit!(SupplyCommitted {
            supplier: supply.supplier,
            timeslot: timeslot_epoch as u64,
            amount: quantity,
        });

        Ok(())
    }


    /// Buyer places bid, escrows quote tokens (USDC) into a program-owned vault (authority = timeslot PDA)
    pub fn place_bid(
        ctx: Context<PlaceBid>,
        page_index: u32,
        price: u64,
        quantity: u64,
        timestamp: i64,
    ) -> Result<()> {
        let ts = &mut ctx.accounts.timeslot;

        require!(matches!(ts.status(), TimeslotStatus::Open), EnergyAuctionError::InvalidTimeslot);
        require!(price > 0 && quantity > 0, EnergyAuctionError::ConstraintViolation);
        require!(price % ts.price_tick == 0, EnergyAuctionError::ConstraintViolation);

        // escrow amount = price * quantity
        let amount = (price as u128)
            .checked_mul(quantity as u128)
            .ok_or(EnergyAuctionError::MathError)?;
        let amount = u64::try_from(amount).map_err(|_| EnergyAuctionError::MathError)?;

        // transfer quote to escrow
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            anchor_spl::token::Transfer {
                from: ctx.accounts.buyer_source.to_account_info(),
                to: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                authority: ctx.accounts.buyer.to_account_info(),
            },
        );
        token::transfer(cpi_ctx, amount)?;

        // append to page
        let page = &mut ctx.accounts.bid_page;
        if page.bids.is_empty() && page.timeslot == Pubkey::default() {
            // first init of this page
            page.timeslot = ts.key();
            page.next_page = None;
        } else {
            // page must belong to this timeslot
            require_keys_eq!(page.timeslot, ts.key(), EnergyAuctionError::ConstraintViolation);
        }

        require!(page.bids.len() < BidPage::MAX_BIDS, EnergyAuctionError::ConstraintViolation);
        page.bids.push(Bid {
            owner: ctx.accounts.buyer.key(),
            price,
            quantity,
            timestamp,
            status: BidStatus::Active as u8,
        });

        ts.total_bids = ts.total_bids.checked_add(quantity).ok_or(EnergyAuctionError::MathError)?;
        Ok(())
    }

    /// Seal a timeslot (freeze order flow)
    pub fn seal_timeslot(ctx: Context<SealTimeslot>) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        let ts = &mut ctx.accounts.timeslot;
        require!(matches!(ts.status(), TimeslotStatus::Open), EnergyAuctionError::InvalidTimeslot);
        ts.status = TimeslotStatus::Sealed as u8;
        Ok(())
    }

    // --- SETTLEMENT FLOW ---

    /// 1. Settle Timeslot: Authority sets the final clearing price and sold quantity.
    /// This instruction only records the outcome; it does not move funds.
    pub fn settle_timeslot(
        ctx: Context<SettleTimeslot>,
        clearing_price: u64,
        total_sold_quantity: u64,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        let ts = &mut ctx.accounts.timeslot;
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        require!(clearing_price > 0, EnergyAuctionError::ConstraintViolation);
        require!(total_sold_quantity <= ts.total_supply, EnergyAuctionError::MathError);

        // Update timeslot state with the auction outcome
        ts.clearing_price = clearing_price;
        ts.total_sold_quantity = total_sold_quantity;
        ts.status = TimeslotStatus::Settled as u8;

        Ok(())
    }

    /// 2. Create Fill Receipt: Authority creates a receipt for each winning buyer.
    pub fn create_fill_receipt(
        ctx: Context<CreateFillReceipt>,
        quantity: u64,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        let ts = &ctx.accounts.timeslot;
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);

        let receipt = &mut ctx.accounts.fill_receipt;
        receipt.buyer = ctx.accounts.buyer.key();
        receipt.timeslot = ts.key();
        receipt.quantity = quantity;
        receipt.clearing_price = ts.clearing_price;
        receipt.redeemed = false;

        Ok(())
    }

    /// 3. Withdraw Proceeds: Seller claims their earnings.
    /// This instruction calculates the fee, sends it to the vault, and sends the net proceeds to the seller.
    pub fn withdraw_proceeds(ctx: Context<WithdrawProceeds>) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let supply = &mut ctx.accounts.supply;
        let global_state = &ctx.accounts.global_state;
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        require!(!supply.claimed, EnergyAuctionError::AlreadyClaimed);

        // Calculate gross proceeds based on the actual sold quantity.
        // NOTE: This assumes a single seller for the MVP.
        let gross_proceeds = (ts.total_sold_quantity as u128)
            .checked_mul(ts.clearing_price as u128)
            .ok_or(EnergyAuctionError::MathError)?;

        // Calculate protocol fee from the gross proceeds
        let protocol_fee = gross_proceeds
            .checked_mul(global_state.fee_bps as u128)
            .ok_or(EnergyAuctionError::MathError)?
            .checked_div(10000)
            .ok_or(EnergyAuctionError::MathError)?;
        
        let net_proceeds = gross_proceeds
            .checked_sub(protocol_fee)
            .ok_or(EnergyAuctionError::MathError)?;

        // PDA signer seeds
        let seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&seeds[..]];

        // Transfer fee to the fee vault
        let cpi_ctx_fee = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                to: ctx.accounts.fee_vault.to_account_info(),
                authority: ts.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx_fee, protocol_fee as u64)?;
        
        // Transfer net proceeds to the seller
        let cpi_ctx_proceeds = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                to: ctx.accounts.seller_proceeds_ata.to_account_info(),
                authority: ts.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx_proceeds, net_proceeds as u64)?;

        supply.claimed = true;
        Ok(())
    }

    /// 4. Redeem Energy & Refund: Buyer claims their won energy and gets a refund for over-bids.
    pub fn redeem_energy_and_refund(
        ctx: Context<RedeemEnergyAndRefund>,
        total_bid_amount_escrowed: u64,
    ) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let receipt = &mut ctx.accounts.fill_receipt;
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        require!(!receipt.redeemed, EnergyAuctionError::AlreadyClaimed);
        require_keys_eq!(receipt.buyer, ctx.accounts.buyer.key(), EnergyAuctionError::Unauthorized);

        // A. Calculate refund
        let cost = (receipt.quantity as u128)
            .checked_mul(receipt.clearing_price as u128)
            .ok_or(EnergyAuctionError::MathError)?;
        let refund_amount = (total_bid_amount_escrowed as u128)
            .checked_sub(cost)
            .ok_or(EnergyAuctionError::MathError)?;

        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        // B. Transfer refund to buyer
        if refund_amount > 0 {
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                    to: ctx.accounts.buyer_quote_ata.to_account_info(),
                    authority: ts.to_account_info(),
                },
                signer_seeds,
            );
            token::transfer(cpi_ctx, refund_amount as u64)?;
        }

        // C. Transfer energy from seller escrows to buyer
        let cpi_ctx_energy = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.seller_escrow.to_account_info(),
                to: ctx.accounts.buyer_energy_ata.to_account_info(),
                authority: ts.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx_energy, receipt.quantity)?;

        receipt.redeemed = true;
        Ok(())
    }
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

    pub quote_mint: Account<'info, Mint>, // USDC or quote token

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
}

/// OpenTimeslot: creates a timeslot PDA
#[derive(Accounts)]
#[instruction(epoch_ts: i64)]
pub struct OpenTimeslot<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,

    #[account(
        init,
        payer = authority,
        space = 8 + Timeslot::LEN,
        seeds = [b"timeslot", &epoch_ts.to_le_bytes()],
        bump
    )]
    pub timeslot: Account<'info, Timeslot>,

    #[account(mut)]
    pub authority: Signer<'info>, // must equal global_state.authority

    pub system_program: Program<'info, System>,
}

/// Seller commits supply for a specific timeslot (one-time)
#[derive(Accounts)]
#[instruction(timeslot_epoch: i64)]
pub struct CommitSupply<'info> {
    pub global_state: Account<'info, GlobalState>,

    #[account(
        mut,
        seeds = [b"timeslot", &timeslot_epoch.to_le_bytes()],
        bump
    )]
    pub timeslot: Account<'info, Timeslot>,

    #[account(
        init,
        payer = signer,
        space = 8 + Supply::LEN,
        seeds = [b"supply", timeslot.key().as_ref(), signer.key().as_ref()],
        bump
    )]
    pub supply: Account<'info, Supply>,

    pub energy_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = seller_source.mint == energy_mint.key() @ EnergyAuctionError::ConstraintViolation,
        constraint = seller_source.owner == signer.key() @ EnergyAuctionError::Unauthorized
    )]
    pub seller_source: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = signer,
        token::mint = energy_mint,
        token::authority = timeslot,
        seeds = [b"seller_escrow", timeslot.key().as_ref(), signer.key().as_ref()],
        bump
    )]
    pub seller_escrow: Account<'info, TokenAccount>,

    #[account(mut)]
    pub signer: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

/// Buyer places a bid into an active bid page
#[derive(Accounts)]
#[instruction(page_index: u32)]
pub struct PlaceBid<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,

    #[account(mut)]
    pub timeslot: Account<'info, Timeslot>,

    #[account(
        init_if_needed,
        payer = buyer,
        token::mint = quote_mint,
        token::authority = timeslot,
        seeds = [b"quote_escrow", timeslot.key().as_ref()],
        bump
    )]
    pub timeslot_quote_escrow: Account<'info, TokenAccount>,

    pub quote_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = buyer_source.mint == quote_mint.key() @ EnergyAuctionError::ConstraintViolation,
        constraint = buyer_source.owner == buyer.key() @ EnergyAuctionError::Unauthorized
    )]
    pub buyer_source: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(
        init_if_needed,
        payer = buyer,
        space = 8 + BidPage::LEN,
        seeds = [b"bid_page", timeslot.key().as_ref(), &page_index.to_le_bytes()],
        bump
    )]
    pub bid_page: Account<'info, BidPage>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SealTimeslot<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub timeslot: Account<'info, Timeslot>,
    pub authority: Signer<'info>,
}

// --- SETTLEMENT CONTEXTS ---

#[derive(Accounts)]
pub struct SettleTimeslot<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct CreateFillReceipt<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    /// CHECK: This is the buyer for whom we are creating the receipt.
    pub buyer: AccountInfo<'info>,
    #[account(
        init,
        payer = authority,
        space = 8 + FillReceipt::LEN,
        seeds = [b"fill_receipt", timeslot.key().as_ref(), buyer.key().as_ref()],
        bump
    )]
    pub fill_receipt: Account<'info, FillReceipt>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawProceeds<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    #[account(
        mut,
        seeds = [b"supply", timeslot.key().as_ref(), seller.key().as_ref()],
        bump
    )]
    pub supply: Account<'info, Supply>,
    #[account(
        mut,
        seeds = [b"quote_escrow", timeslot.key().as_ref()],
        bump
    )]
    pub timeslot_quote_escrow: Account<'info, TokenAccount>,
    #[account(
        mut,
        seeds = [b"fee_vault"],
        bump
    )]
    pub fee_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub seller_proceeds_ata: Account<'info, TokenAccount>,
    #[account(mut, address = supply.supplier)]
    pub seller: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RedeemEnergyAndRefund<'info> {
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    #[account(
        mut,
        seeds = [b"fill_receipt", timeslot.key().as_ref(), buyer.key().as_ref()],
        bump,
        has_one = buyer @ EnergyAuctionError::Unauthorized
    )]
    pub fill_receipt: Account<'info, FillReceipt>,
    #[account(
        mut,
        seeds = [b"quote_escrow", timeslot.key().as_ref()],
        bump
    )]
    pub timeslot_quote_escrow: Account<'info, TokenAccount>,
    #[account(mut)]
    pub buyer_quote_ata: Account<'info, TokenAccount>,
    #[account(mut)]
    pub buyer_energy_ata: Account<'info, TokenAccount>,
    /// CHECK: This is a seller's energy escrow. A real implementation would iterate over many.
    #[account(mut)]
    pub seller_escrow: Account<'info, TokenAccount>,
    #[account(mut)]
    pub buyer: Signer<'info>,
    pub token_program: Program<'info, Token>,
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
    pub timeslot: Pubkey,     // timeslot account
    pub amount: u64,          // Amount committed (lots)
    pub reserve_price: u64,   // min acceptable price per lot (quote units)
    pub bump: u8,             // PDA bump
    pub energy_mint: Pubkey,  // energy token mint
    pub escrow_vault: Pubkey, // escrow token account for energy
    pub claimed: bool,        // Has the seller withdrawn proceeds?
}

impl Supply {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 1 + 32 + 32 + 1;
}

/// Auction round container
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
    pub clearing_price: u64,  // Final price determined after sealing
    pub total_sold_quantity: u64, // Final quantity sold in the auction
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
        + 1 + 32              // tail_page (Option<Pubkey>)
        + 8                   // clearing_price
        + 8;                  // total_sold_quantity

    pub fn status(&self) -> TimeslotStatus {
        match self.status {
            0 => TimeslotStatus::Pending,
            1 => TimeslotStatus::Open,
            2 => TimeslotStatus::Sealed,
            3 => TimeslotStatus::Settled,
            _ => TimeslotStatus::Cancelled,
        }
    }
}

#[repr(u8)]
pub enum TimeslotStatus {
    Pending = 0,
    Open = 1,
    Sealed = 2,
    Settled = 3,
    Cancelled = 4,
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

#[repr(u8)]
pub enum BidStatus { Active = 0, Cancelled = 1, Filled = 2 }

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
    pub bids: Vec<Bid>,           // fixed max length (MVP: 150)
    pub next_page: Option<Pubkey>,
}

impl BidPage {
    pub const MAX_BIDS: usize = 150;
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
    #[msg("Invalid authority for this operation")]
    InvalidAuthority,
    #[msg("Supply already committed for this seller and timeslot")]
    DuplicateSupply,
    #[msg("Timeslot is not in the correct state for this operation")]
    InvalidTimeslot,
    #[msg("Insufficient token balance to commit supply")]
    InsufficientBalance,
    #[msg("Math overflow/underflow error")]
    MathError,
    #[msg("Invalid escrow vault account")]
    InvalidEscrowVault,
    #[msg("Unauthorized signer for this transaction")]
    Unauthorized,
    #[msg("Invalid global state account provided")]
    InvalidGlobalState,
    #[msg("Account constraint violated")]
    ConstraintViolation,
    #[msg("Proceeds or refund have already been claimed")]
    AlreadyClaimed,
}
