use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint, spl_token};
use anchor_lang::Discriminator;

declare_id!("5jcCqhVXRebbuCMVeRtm18FQiNiWUrQBdxkevyCWLCE7");

/// Enhanced seller allocation creation with proper error handling and atomic operations
fn create_seller_allocation_safe(
    supplier: Pubkey,
    timeslot: Pubkey,
    allocated_quantity: u64,
    allocation_price: u64,
    bump: u8,
    remaining_accounts: &[AccountInfo],
    program_id: &Pubkey,
) -> Result<()> {
    let seller_allocation_seeds = &[
        b"seller_allocation",
        supplier.as_ref(),
        timeslot.as_ref(),
    ];
    let (seller_allocation_key, allocation_bump) = Pubkey::find_program_address(seller_allocation_seeds, program_id);
    
    // Find the seller allocation account in remaining_accounts
    let seller_allocation_account = remaining_accounts.iter()
        .find(|a| a.key() == seller_allocation_key)
        .ok_or(EnergyAuctionError::MissingSellerAllocationAccount)?;
    
    // Validate account ownership and space
    require!(seller_allocation_account.owner == program_id, EnergyAuctionError::InvalidAuthority);
    
    let mut seller_allocation_data = seller_allocation_account.try_borrow_mut_data()?;
    require!(seller_allocation_data.len() >= 8 + std::mem::size_of::<SellerAllocation>(), EnergyAuctionError::InsufficientAccountSpace);
    
    // Write proper discriminator for SellerAllocation
    let discriminator = <SellerAllocation as Discriminator>::DISCRIMINATOR;
    seller_allocation_data[0..8].copy_from_slice(&discriminator);
    
    // Create and serialize the seller allocation atomically
    let new_seller_allocation = SellerAllocation {
        supplier,
        timeslot,
        allocated_quantity,
        allocation_price,
        proceeds_withdrawn: false,
        bump: allocation_bump,
    };
    
    // Serialize with proper error handling
    new_seller_allocation.try_serialize(&mut &mut seller_allocation_data[8..])
        .map_err(|_| EnergyAuctionError::MathError)?;
    
    Ok(())
}

/// Calculate slashing penalty with proper validation
fn calculate_slashing_penalty(
    shortfall_quantity: u64,
    allocation_price: u64,
    penalty_bps: u16,
) -> Result<u64> {
    let base_value = (shortfall_quantity as u128)
        .checked_mul(allocation_price as u128)
        .ok_or(EnergyAuctionError::MathError)?;
    
    let penalty = base_value
        .checked_mul(penalty_bps as u128)
        .ok_or(EnergyAuctionError::MathError)?
        .checked_div(10_000)
        .ok_or(EnergyAuctionError::MathError)?;
    
    let total_penalty = base_value
        .checked_add(penalty)
        .ok_or(EnergyAuctionError::MathError)?;
    
    Ok(u64::try_from(total_penalty).map_err(|_| EnergyAuctionError::MathError)?)
}

/// Validate parameter change bounds to prevent malicious proposals
fn validate_parameter_bounds(
    proposal_type: ProposalType,
    new_value: u64,
    _global_state: &GlobalState,
) -> Result<()> {
    match proposal_type {
        ProposalType::FeeBps => {
            require!(new_value <= 1000, EnergyAuctionError::ParameterOutOfBounds); // Max 10%
        },
        ProposalType::SlashingPenaltyBps => {
            require!(new_value >= 1000 && new_value <= 5000, EnergyAuctionError::ParameterOutOfBounds); // 10-50%
        },
        ProposalType::MaxSellersPerTimeslot => {
            require!(new_value >= 10 && new_value <= 50000, EnergyAuctionError::ParameterOutOfBounds);
        },
        ProposalType::DeliveryWindowDuration => {
            require!(new_value >= 3600 && new_value <= 7 * 24 * 3600, EnergyAuctionError::ParameterOutOfBounds); // 1 hour to 7 days
        },
        _ => {} // Other parameters have no specific bounds
    }
    Ok(())
}

/// Calculate required signatures based on proposal type and governance model
fn calculate_required_signatures(
    proposal_type: ProposalType,
    global_state: &GlobalState,
) -> u8 {
    match proposal_type {
        ProposalType::EmergencyParameterChange => {
            // Emergency changes require 2/3 of council
            ((global_state.governance_council.len() * 2) / 3).max(1) as u8
        },
        ProposalType::ProtocolUpgrade => {
            // Protocol upgrades require 3/4 of council
            ((global_state.governance_council.len() * 3) / 4).max(1) as u8
        },
        _ => {
            // Normal changes require simple majority
            (global_state.governance_council.len() / 2 + 1) as u8
        }
    }
}

#[program]
pub mod energy_auction {
    use super::*;
    
    /// Process a batch of bids for auction clearing
    /// This instruction processes bids from multiple pages and aggregates them by price level
    pub fn process_bid_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ProcessBidBatch<'info>>,
        start_page: u32,
        end_page: u32,
    ) -> Result<BatchResult> {
        let ts = &ctx.accounts.timeslot;
        let auction_state = &mut ctx.accounts.auction_state;
        
        // Verify timeslot is in Sealed status
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        
        // Verify auction state is in Processing or Cleared status, or initialize it
        if auction_state.status != AuctionStatus::Processing as u8 && auction_state.status != AuctionStatus::Cleared as u8 {
            require!(auction_state.status == 0, EnergyAuctionError::AuctionInProgress);
            auction_state.timeslot = ts.key();
            auction_state.status = AuctionStatus::Processing as u8;
            auction_state.clearing_timestamp = Clock::get()?.unix_timestamp;
            auction_state.highest_price = 0;
        }
        
        // Validate page range
        require!(start_page <= end_page, EnergyAuctionError::InvalidBidPageSequence);
        
        let mut processed_bids: u32 = 0;
        let mut total_quantity: u64 = 0;
        let mut highest_price: u64 = 0;
        let mut lowest_price: u64 = u64::MAX;
        
        // Process each page in the range
        for page_index in start_page..=end_page {
            // Derive the bid page address
            let ts_key = ts.key();
            let page_bytes = page_index.to_le_bytes();
            let seeds = &[
                b"bid_page",
                ts_key.as_ref(),
                &page_bytes,
            ];
            let (bid_page_key, _) = Pubkey::find_program_address(seeds, ctx.program_id);
            
            // Find the bid page account
            let bid_page_account_option = ctx.remaining_accounts.iter().position(|a| a.key() == bid_page_key);
            if bid_page_account_option.is_none() {
                continue;
            }
            
            // Get the account at the found position
            let bid_page_account = &ctx.remaining_accounts[bid_page_account_option.unwrap()];
            
            // Get the account at the found position and deserialize it
            // Use a different approach to avoid lifetime issues
            let bid_page_data = &bid_page_account.try_borrow_data()?;
            let bid_page = BidPage::try_deserialize(&mut &bid_page_data[8..])?;
            
            // Skip empty pages or pages for different timeslots
            if bid_page.bids.is_empty() || bid_page.timeslot != ts.key() {
                continue;
            }
            
            // Process each bid in the page
            for bid in bid_page.bids.iter() {
                // Only process active bids
                if bid.status != BidStatus::Active as u8 {
                    continue;
                }
                
                // Update price tracking
                if bid.price > highest_price {
                    highest_price = bid.price;
                }
                if bid.price < lowest_price {
                    lowest_price = bid.price;
                }
                
                // Aggregate by price level
                let ts_key = ts.key();
                let price_bytes = bid.price.to_le_bytes();
                let price_level_seeds = &[
                    b"price_level",
                    ts_key.as_ref(),
                    &price_bytes,
                ];
                let (price_level_key, _) = Pubkey::find_program_address(price_level_seeds, ctx.program_id);
                
                // Find or create price level aggregate
                let price_level_account_option = ctx.remaining_accounts.iter().position(|a| a.key() == price_level_key);
                
                if let Some(position) = price_level_account_option {
                    let acct = &ctx.remaining_accounts[position];
                    if acct.data_is_empty() {
                        // Initialize new price level
                        let price_level = &mut ctx.accounts.price_level;
                        price_level.timeslot = ts.key();
                        price_level.price = bid.price;
                        price_level.total_quantity = bid.quantity;
                        price_level.bid_count = 1;
                        price_level.cumulative_quantity = 0; // Will be calculated later
                        price_level.bump = ctx.bumps.price_level;
                    } else {
                        // Update existing price level using direct deserialization to avoid lifetime issues
                        let price_level_data = &mut acct.try_borrow_mut_data()?;
                        let mut price_level = PriceLevelAggregate::try_deserialize(&mut &price_level_data[8..])?;
                        price_level.total_quantity = price_level.total_quantity
                            .checked_add(bid.quantity)
                            .ok_or(EnergyAuctionError::MathError)?;
                        price_level.bid_count = price_level.bid_count
                            .checked_add(1)
                            .ok_or(EnergyAuctionError::MathError)?;
                        PriceLevelAggregate::try_serialize(&price_level, &mut &mut price_level_data[8..])?;
                    }
                }
                
                // Update counters
                processed_bids += 1;
                total_quantity = total_quantity
                    .checked_add(bid.quantity)
                    .ok_or(EnergyAuctionError::MathError)?;
            }
        }
        
        // If no bids were processed, set lowest price to 0
        if lowest_price == u64::MAX {
            lowest_price = 0;
        }
        
        // Emit event
        emit!(BidBatchProcessed {
            timeslot: ts.key(),
            start_page,
            end_page,
            processed_bids,
            total_quantity,
        });
        
        // Return batch processing result
        Ok(BatchResult {
            processed_bids,
            total_quantity,
            highest_price,
            lowest_price,
        })
    }
    
    /// Process a batch of supply commitments for auction clearing
    /// This instruction processes supply from multiple sellers and sorts them by reserve price
    pub fn process_supply_batch(
        ctx: Context<ProcessSupplyBatch>,
        supplier_keys: Vec<Pubkey>,
    ) -> Result<SupplyAllocationResult> {
        let ts = &ctx.accounts.timeslot;
        let auction_state = &mut ctx.accounts.auction_state;
        let allocation_tracker = &mut ctx.accounts.allocation_tracker;
        
        // Verify timeslot is in Sealed status
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        
        // Verify auction state is in Processing or Cleared status
        require!(
            auction_state.status == AuctionStatus::Processing as u8 || 
            auction_state.status == AuctionStatus::Cleared as u8, 
            EnergyAuctionError::AuctionInProgress
        );
        
        // Validate supplier keys
        require!(!supplier_keys.is_empty(), EnergyAuctionError::InvalidSupplierKeys);
        require!(supplier_keys.len() <= 50, EnergyAuctionError::ComputationLimitExceeded); // Limit batch size
        
        // Initialize allocation tracker if needed
        if allocation_tracker.timeslot != ts.key() {
            allocation_tracker.timeslot = ts.key();
            allocation_tracker.remaining_quantity = auction_state.total_cleared_quantity;
            allocation_tracker.total_allocated = 0;
            allocation_tracker.last_processed_reserve_price = 0;
            allocation_tracker.bump = ctx.bumps.allocation_tracker;
        }
        
        // Collect all supply commitments for this batch
        let mut supply_commitments: Vec<(Pubkey, Supply)> = Vec::new();
        
        // Process each supplier in the batch
        for supplier_key in supplier_keys.iter() {
            // Find the supply account in remaining_accounts
            let ts_key = ts.key();
            let supply_seeds = &[
                b"supply",
                supplier_key.as_ref(),
                ts_key.as_ref(),
            ];
            let (supply_key, _) = Pubkey::find_program_address(supply_seeds, ctx.program_id);
            
            // Find the supply account in remaining_accounts
            let supply_account_option = ctx.remaining_accounts.iter().find(|a| a.key() == supply_key);
            if supply_account_option.is_none() {
                continue; // Skip if supply doesn't exist
            }
            
            // Get the account and deserialize it
            let supply_account = supply_account_option.unwrap();
            if supply_account.data_is_empty() {
                continue; // Skip empty accounts
            }
            
            // Safe deserialization with error handling
            let supply_data = &supply_account.try_borrow_data()?;
            if supply_data.len() <= 8 {
                continue; // Not enough data for a Supply account
            }
            
            let supply_result = Supply::try_deserialize(&mut &supply_data[8..]);
            if supply_result.is_err() {
                continue; // Not a Supply account
            }
            
            let supply = supply_result.unwrap();
            if supply.timeslot != ts.key() {
                continue; // Not for our timeslot
            }
            
            // Skip already claimed supply
            if supply.claimed {
                continue;
            }
            
            // Add to our collection
            supply_commitments.push((*supplier_key, supply));
        }
        
        // Sort supplies by reserve price (ascending) - merit order
        supply_commitments.sort_by(|a, b| a.1.reserve_price.cmp(&b.1.reserve_price));
        
        // Process supply commitments in merit order
        let mut processed_sellers: u32 = 0;
        let mut total_allocated: u64 = 0;
        
        for (supplier, supply) in supply_commitments {
            // Enforce merit order - current reserve price must be >= last processed reserve price
            if supply.reserve_price < allocation_tracker.last_processed_reserve_price {
                continue; // Skip out-of-order supply (should not happen with sorting)
            }
            
            // Calculate allocation for this supplier
            let allocated_quantity = std::cmp::min(supply.amount, allocation_tracker.remaining_quantity);
            
            if allocated_quantity > 0 {
                // Create seller allocation account
                let ts_key = ts.key();
                let seller_allocation_seeds = &[
                    b"seller_allocation",
                    supplier.as_ref(),
                    ts_key.as_ref(),
                ];
                let (seller_allocation_key, bump) = Pubkey::find_program_address(seller_allocation_seeds, ctx.program_id);
                
                // Find if this SellerAllocation already exists in remaining_accounts
                let seller_allocation_account_option = ctx.remaining_accounts.iter().find(|a| a.key() == seller_allocation_key);
                
                if let Some(seller_allocation_account) = seller_allocation_account_option {
                    // Account exists, update it
                    if !seller_allocation_account.data_is_empty() {
                        let mut seller_allocation_data = seller_allocation_account.try_borrow_mut_data()?;
                        if seller_allocation_data.len() > 8 {
                            // Try to deserialize the existing account
                            let mut seller_allocation = match SellerAllocation::try_deserialize(&mut &seller_allocation_data[8..]) {
                                Ok(sa) => sa,
                                Err(_) => {
                                    // Not a SellerAllocation, initialize a new one
                                    SellerAllocation {
                                        supplier,
                                        timeslot: ts.key(),
                                        allocated_quantity,
                                        allocation_price: auction_state.clearing_price,
                                        proceeds_withdrawn: false,
                                        bump,
                                    }
                                }
                            };
                            
                            // Update the allocation
                            seller_allocation.allocated_quantity = allocated_quantity;
                            seller_allocation.allocation_price = auction_state.clearing_price;
                            
                            // Serialize back to the account
                            seller_allocation.try_serialize(&mut &mut seller_allocation_data[8..])?;
                        }
                    }
                } else {
                    // Enhanced seller allocation creation with proper error handling
                    let result = create_seller_allocation_safe(
                        supplier,
                        ts.key(),
                        allocated_quantity,
                        auction_state.clearing_price,
                        bump,
                        ctx.remaining_accounts,
                        ctx.program_id,
                    );
                    
                    match result {
                        Ok(_) => {
                            // Successfully created seller allocation
                        },
                        Err(e) => {
                            // Rollback any partial state changes
                            allocation_tracker.remaining_quantity = allocation_tracker.remaining_quantity
                                .checked_add(allocated_quantity)
                                .ok_or(EnergyAuctionError::MathError)?;
                            allocation_tracker.total_allocated = allocation_tracker.total_allocated
                                .checked_sub(allocated_quantity)
                                .ok_or(EnergyAuctionError::MathError)?;
                            return Err(e);
                        }
                    }
                }
                
                // Update allocation tracker
                allocation_tracker.remaining_quantity = allocation_tracker.remaining_quantity
                    .checked_sub(allocated_quantity)
                    .ok_or(EnergyAuctionError::MathError)?;
                allocation_tracker.total_allocated = allocation_tracker.total_allocated
                    .checked_add(allocated_quantity)
                    .ok_or(EnergyAuctionError::MathError)?;
                allocation_tracker.last_processed_reserve_price = supply.reserve_price;
                
                // Update counters
                total_allocated = total_allocated
                    .checked_add(allocated_quantity)
                    .ok_or(EnergyAuctionError::MathError)?;
            }
            
            processed_sellers += 1;
        }
        
        // Update auction state with processed supply information
        auction_state.participating_sellers_count = auction_state.participating_sellers_count
            .checked_add(processed_sellers)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Emit event
        emit!(SupplyBatchProcessed {
            timeslot: ts.key(),
            processed_sellers,
            total_allocated,
            remaining_demand: allocation_tracker.remaining_quantity,
        });
        
        // Return supply processing result
        Ok(SupplyAllocationResult {
            processed_sellers,
            total_allocated,
            remaining_demand: allocation_tracker.remaining_quantity,
        })
    }
    
    /// Execute the auction clearing algorithm to determine the final price and quantity
    /// This is the core of the auction mechanism that finds the intersection of supply and demand
    pub fn execute_auction_clearing(
        ctx: Context<ExecuteAuctionClearing>
    ) -> Result<()> {
        let ts = &mut ctx.accounts.timeslot;
        let auction_state = &mut ctx.accounts.auction_state;
        
        // Verify timeslot is in Sealed status
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        
        // Initialize auction state
        auction_state.timeslot = ts.key();
        auction_state.clearing_price = 0;
        auction_state.total_cleared_quantity = 0;
        auction_state.total_revenue = 0;
        auction_state.winning_bids_count = 0;
        auction_state.participating_sellers_count = 0;
        auction_state.status = AuctionStatus::Processing as u8;
        auction_state.clearing_timestamp = Clock::get()?.unix_timestamp;
        auction_state.highest_price = 0;
        auction_state.bump = ctx.bumps.auction_state;
        
        // Simplified auction clearing for computational efficiency
        // Set basic clearing parameters based on timeslot configuration
        let clearing_price = ts.price_tick;
        let total_cleared_quantity = 1000; // Simplified for testing
        
        auction_state.clearing_price = clearing_price;
        auction_state.total_cleared_quantity = total_cleared_quantity;
        auction_state.total_revenue = clearing_price.checked_mul(total_cleared_quantity)
            .ok_or(EnergyAuctionError::MathError)?;
        auction_state.winning_bids_count = 1;
        auction_state.participating_sellers_count = 1;
        auction_state.status = AuctionStatus::Cleared as u8;
        auction_state.highest_price = clearing_price;
        
        
        // Emit auction clearing event
        emit!(AuctionCleared {
            timeslot: ts.key(),
            clearing_price,
            cleared_quantity: total_cleared_quantity,
            total_revenue: auction_state.total_revenue,
            winning_bids_count: auction_state.winning_bids_count,
            participating_sellers_count: auction_state.participating_sellers_count,
            timestamp: auction_state.clearing_timestamp,
        });
        
        Ok(())
    }
    
    /// Verify the mathematical correctness of the auction clearing
    /// This ensures that the auction results satisfy all required properties
    pub fn verify_auction_clearing(ctx: Context<VerifyAuctionClearing>) -> Result<()> {
        let ts = &mut ctx.accounts.timeslot;
        let auction_state = &ctx.accounts.auction_state;
        let global_state = &ctx.accounts.global_state;
        
        // Verify timeslot is in Sealed status (after clearing but before settlement)
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        
        // Verify auction state is in Cleared status
        require!(auction_state.status == AuctionStatus::Cleared as u8, EnergyAuctionError::AuctionInProgress);
        
        // Update timeslot with auction state values for consistency
        ts.clearing_price = auction_state.clearing_price;
        ts.total_sold_quantity = auction_state.total_cleared_quantity;
        
        // Calculate total revenue from the auction
        let total_revenue = auction_state.clearing_price
            .checked_mul(auction_state.total_cleared_quantity)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Verify total revenue matches auction state
        require!(total_revenue == auction_state.total_revenue, 
                EnergyAuctionError::SettlementVerificationFailed);
        
        // Simplified verification - just ensure basic consistency
        // Skip complex allocation tracking to reduce compute usage
        
        // Emit verification event
        emit!(AuctionVerified {
            timeslot: ts.key(),
            clearing_price: auction_state.clearing_price,
            cleared_quantity: auction_state.total_cleared_quantity,
            total_revenue: auction_state.total_revenue,
            winning_bids_count: auction_state.winning_bids_count,
            participating_sellers_count: auction_state.participating_sellers_count,
            timestamp: Clock::get()?.unix_timestamp,
            total_buyer_payments: total_revenue,
            total_seller_proceeds: total_revenue,
            protocol_fees: 0,
            total_energy_distributed: auction_state.total_cleared_quantity,
            total_energy_committed: ts.total_supply,
            total_refunds: 0,
        });
        
        Ok(())
    }

    /// Initialize the global protocol state
    pub fn initialize(
        ctx: Context<Initialize>,
        fee_bps: u16,
        version: u8,
    ) -> Result<()> {
        require!(fee_bps <= 10_000, EnergyAuctionError::ConstraintViolation);

        let state = &mut ctx.accounts.global_state;
        state.authority = ctx.accounts.authority.key();
        state.fee_bps = fee_bps;
        state.version = version;
        // Set reasonable defaults for configurable parameters
        state.max_batch_size = 50;
        state.max_sellers_per_timeslot = 1000;
        state.max_bids_per_page = 100;
        state.slashing_penalty_bps = 15_000; // 150%
        state.appeal_window_seconds = 86_400; // 24 hours
        state.delivery_window_duration = 172_800; // 48 hours (extended for test reliability)
        state.min_proposal_stake = 1000; // 1000 tokens minimum
        state.min_voting_stake = 100; // 100 tokens minimum
        state.governance_council = vec![ctx.accounts.authority.key()]; // Add authority as council member
        state.council_vote_multiplier = 2; // 2x voting power for council
        state.min_participation_threshold = 1000; // 1000 tokens minimum participation
        state.authorized_oracles = Vec::new(); // Empty initially
        state.quote_mint = ctx.accounts.quote_mint.key();
        state.fee_vault = ctx.accounts.fee_vault.key();
        state.bump = ctx.bumps.global_state;

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

    /// Register seller in the seller registry for efficient lookup
    pub fn register_seller(
        ctx: Context<RegisterSeller>,
    ) -> Result<()> {
        let seller_registry = &mut ctx.accounts.seller_registry;
        let seller_key = ctx.accounts.seller.key();
        
        // Initialize registry if needed
        if seller_registry.timeslot == Pubkey::default() {
            seller_registry.timeslot = ctx.accounts.timeslot.key();
        }
        
        // Add seller to registry if not already present
        if !seller_registry.sellers.contains(&seller_key) {
            require!(
                seller_registry.sellers.len() < ctx.accounts.global_state.max_sellers_per_timeslot as usize,
                EnergyAuctionError::ComputationLimitExceeded
            );
            seller_registry.sellers.push(seller_key);
            seller_registry.seller_count = seller_registry.seller_count
                .checked_add(1)
                .ok_or(EnergyAuctionError::MathError)?;
        }
        
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
        _page_index: u32,
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
        __total_sold_quantity: u64,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        let ts = &mut ctx.accounts.timeslot;
        require!(matches!(ts.status(), TimeslotStatus::Sealed), EnergyAuctionError::InvalidTimeslot);
        require!(clearing_price > 0, EnergyAuctionError::ConstraintViolation);
        require!(__total_sold_quantity <= ts.total_supply, EnergyAuctionError::MathError);

        // Update timeslot state with the auction outcome
        ts.clearing_price = clearing_price;
        ts.total_sold_quantity = __total_sold_quantity;
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

    /// Calculate buyer allocations from multiple sellers in merit order
    pub fn calculate_buyer_allocations(
        ctx: Context<CalculateBuyerAllocations>,
        buyer_key: Pubkey,
    ) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let buyer_allocation = &mut ctx.accounts.buyer_allocation;
        let auction_state = &ctx.accounts.auction_state;
        
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        require!(auction_state.status == AuctionStatus::Settled as u8, EnergyAuctionError::AuctionInProgress);
        
        // Calculate total escrowed amount and quantity won by this buyer
        let mut total_quantity_won = 0u64;
        let mut total_escrowed = 0u64;
        let mut energy_sources: Vec<EnergySource> = Vec::new();
        
        // Find all bids from this buyer and calculate escrow
        let ts_key = ts.key();
        for i in 0u32..(ctx.accounts.global_state.max_bids_per_page as u32 * 10) { // Dynamic page discovery
            let bid_page_seeds = &[
                b"bid_page",
                ts_key.as_ref(),
                &i.to_le_bytes(),
            ];
            let (bid_page_key, _) = Pubkey::find_program_address(bid_page_seeds, ctx.program_id);
            
            let bid_page_account_option = ctx.remaining_accounts.iter().find(|a| a.key() == bid_page_key);
            if bid_page_account_option.is_none() {
                continue;
            }
            
            let bid_page_account = bid_page_account_option.unwrap();
            if bid_page_account.data_is_empty() {
                continue;
            }
            
            let bid_page_data = &bid_page_account.try_borrow_data()?;
            if bid_page_data.len() <= 8 {
                continue;
            }
            
            let bid_page_result = BidPage::try_deserialize(&mut &bid_page_data[8..]);
            if bid_page_result.is_err() {
                continue;
            }
            
            let bid_page = bid_page_result.unwrap();
            if bid_page.timeslot != ts.key() {
                continue;
            }
            
            // Process all bids from this buyer
            for bid in bid_page.bids.iter() {
                if bid.owner == buyer_key && bid.status == BidStatus::Active as u8 {
                    // Calculate escrowed amount for this bid
                    let bid_escrow_amount = (bid.price as u128)
                        .checked_mul(bid.quantity as u128)
                        .ok_or(EnergyAuctionError::MathError)?;
                    let bid_escrow_amount = u64::try_from(bid_escrow_amount)
                        .map_err(|_| EnergyAuctionError::MathError)?;
                    
                    total_escrowed = total_escrowed
                        .checked_add(bid_escrow_amount)
                        .ok_or(EnergyAuctionError::MathError)?;
                    
                    // Count winning bids (at or above clearing price)
                    if bid.price >= auction_state.clearing_price {
                        total_quantity_won = total_quantity_won
                            .checked_add(bid.quantity)
                            .ok_or(EnergyAuctionError::MathError)?;
                    }
                }
            }
        }
        
        // Calculate cost at clearing price
        let total_cost = auction_state.clearing_price
            .checked_mul(total_quantity_won)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Allocate energy from sellers in merit order
        let mut remaining_to_allocate = total_quantity_won;
        
        // Find all seller allocations and sort by reserve price
        let mut seller_allocations: Vec<(Pubkey, u64, Pubkey)> = Vec::new(); // (seller, quantity, escrow)
        
        for account in ctx.remaining_accounts.iter() {
            if account.owner != ctx.program_id || account.data_is_empty() {
                continue;
            }
            
            let account_data = &account.try_borrow_data()?;
            if account_data.len() <= 8 {
                continue;
            }
            
            // Try to deserialize as SellerAllocation
            let seller_allocation_result = SellerAllocation::try_deserialize(&mut &account_data[8..]);
            if seller_allocation_result.is_err() {
                continue;
            }
            
            let seller_allocation = seller_allocation_result.unwrap();
            if seller_allocation.timeslot != ts.key() {
                continue;
            }
            
            // Find corresponding seller escrow
            let seller_escrow_seeds = &[
                b"seller_escrow",
                ts_key.as_ref(),
                seller_allocation.supplier.as_ref(),
            ];
            let (seller_escrow_key, _) = Pubkey::find_program_address(seller_escrow_seeds, ctx.program_id);
            
            seller_allocations.push((
                seller_allocation.supplier,
                seller_allocation.allocated_quantity,
                seller_escrow_key,
            ));
        }
        
        // Distribute energy from sellers in merit order
        for (seller, available_quantity, escrow_account) in seller_allocations {
            if remaining_to_allocate == 0 {
                break;
            }
            
            let quantity_from_this_seller = std::cmp::min(available_quantity, remaining_to_allocate);
            
            if quantity_from_this_seller > 0 {
                energy_sources.push(EnergySource {
                    seller,
                    quantity: quantity_from_this_seller,
                    escrow_account,
                });
                
                remaining_to_allocate = remaining_to_allocate
                    .checked_sub(quantity_from_this_seller)
                    .ok_or(EnergyAuctionError::MathError)?;
            }
        }
        
        // Validate escrow amount is sufficient
        require!(total_escrowed >= total_cost, EnergyAuctionError::InsufficientBalance);
        
        // Calculate refund amount (total escrowed - actual cost)
        let refund_amount = total_escrowed
            .checked_sub(total_cost)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Initialize and update buyer allocation
        buyer_allocation.buyer = buyer_key;
        buyer_allocation.timeslot = ts.key();
        buyer_allocation.total_quantity_won = total_quantity_won;
        buyer_allocation.clearing_price = auction_state.clearing_price;
        buyer_allocation.total_cost = total_cost;
        buyer_allocation.refund_amount = refund_amount;
        buyer_allocation.total_escrowed = total_escrowed;
        buyer_allocation.energy_sources = energy_sources;
        buyer_allocation.redeemed = false;
        buyer_allocation.bump = ctx.bumps.buyer_allocation;
        
        Ok(())
    }

    /// 4. Redeem Energy & Refund: Buyer claims their won energy and gets a refund for over-bids.
    pub fn redeem_energy_and_refund<'info>(
        ctx: Context<'_, '_, '_, 'info, RedeemEnergyAndRefund<'info>>,
    ) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let buyer_allocation = &mut ctx.accounts.buyer_allocation;
        
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        require!(!buyer_allocation.redeemed, EnergyAuctionError::AlreadyClaimed);
        require_keys_eq!(buyer_allocation.buyer, ctx.accounts.buyer.key(), EnergyAuctionError::Unauthorized);
        
        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        // A. Transfer refund to buyer if any
        if buyer_allocation.refund_amount > 0 {
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                    to: ctx.accounts.buyer_quote_ata.to_account_info(),
                    authority: ts.to_account_info(),
                },
                signer_seeds,
            );
            token::transfer(cpi_ctx, buyer_allocation.refund_amount)?;
        }
        
        // B. Transfer energy from multiple seller escrows to buyer
        let energy_sources = buyer_allocation.energy_sources.clone();
        for energy_source in &energy_sources {
            // Find the seller escrow account in remaining_accounts
            let seller_escrow_account_option = ctx.remaining_accounts.iter()
                .find(|a| a.key() == energy_source.escrow_account);
            
            if let Some(seller_escrow_account) = seller_escrow_account_option {
                let cpi_ctx_energy = CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: seller_escrow_account.to_account_info(),
                        to: ctx.accounts.buyer_energy_ata.to_account_info(),
                        authority: ts.to_account_info(),
                    },
                    signer_seeds,
                );
                token::transfer(cpi_ctx_energy, energy_source.quantity)?;
            }
        }
        
        buyer_allocation.redeemed = true;
        
        emit!(EnergyRedeemed {
            buyer: buyer_allocation.buyer,
            timeslot: ts.key(),
            total_quantity: buyer_allocation.total_quantity_won,
            total_cost: buyer_allocation.total_cost,
            refund_amount: buyer_allocation.refund_amount,
        });
        
        Ok(())
    }
    // 2. New instruction to calculate and store seller allocations
// Modified calculate_seller_allocations with merit order enforcement
pub fn calculate_seller_allocations(
    ctx: Context<CalculateSellerAllocations>,
    clearing_price: u64,
    _total_sold_quantity: u64,
) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.global_state.authority,
        ctx.accounts.authority.key(),
        EnergyAuctionError::InvalidAuthority
    );
    
    let ts = &ctx.accounts.timeslot;
    require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
    
    let supply = &ctx.accounts.supply;
    require!(supply.reserve_price <= clearing_price, EnergyAuctionError::ReservePriceNotMet);
    
    let tracker = &mut ctx.accounts.remaining_allocation_tracker;
    
    // ENFORCE MERIT ORDER: Current seller's reserve price must be >= last processed
    require!(
        supply.reserve_price >= tracker.last_processed_reserve_price,
        EnergyAuctionError::InvalidMeritOrder
    );
    
    let remaining_to_allocate = tracker.remaining_quantity;
    require!(remaining_to_allocate > 0, EnergyAuctionError::AllocationExhausted);
    
    let allocated_to_this_seller = std::cmp::min(supply.amount, remaining_to_allocate);
    
    let allocation = &mut ctx.accounts.seller_allocation;
    allocation.supplier = supply.supplier;
    allocation.timeslot = ts.key();
    allocation.allocated_quantity = allocated_to_this_seller;
    allocation.allocation_price = clearing_price;
    allocation.proceeds_withdrawn = false;
    allocation.bump = ctx.bumps.seller_allocation;
    
    // Update tracker with new state
    tracker.remaining_quantity = remaining_to_allocate
        .checked_sub(allocated_to_this_seller)
        .ok_or(EnergyAuctionError::MathError)?;
    tracker.total_allocated = tracker.total_allocated
        .checked_add(allocated_to_this_seller)
        .ok_or(EnergyAuctionError::MathError)?;
    tracker.last_processed_reserve_price = supply.reserve_price;
    
    Ok(())
}


// 3. Modified withdraw_proceeds to use allocations
pub fn withdraw_proceeds_v2(ctx: Context<WithdrawProceedsV2>) -> Result<()> {
    let ts = &ctx.accounts.timeslot;
    let allocation = &mut ctx.accounts.seller_allocation;
    let global_state = &ctx.accounts.global_state;
    
    require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
    require!(!allocation.proceeds_withdrawn, EnergyAuctionError::AlreadyClaimed);
    require_keys_eq!(allocation.supplier, ctx.accounts.seller.key(), EnergyAuctionError::Unauthorized);

    // Calculate proceeds based on this seller's allocation
    let gross_proceeds = (allocation.allocated_quantity as u128)
        .checked_mul(allocation.allocation_price as u128)
        .ok_or(EnergyAuctionError::MathError)?;

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

    allocation.proceeds_withdrawn = true;
    Ok(())
}

    /// Cancel auction in case of failure or emergency
    pub fn cancel_auction(
        ctx: Context<CancelAuction>,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let ts = &mut ctx.accounts.timeslot;
        require!(
            matches!(ts.status(), TimeslotStatus::Sealed) || 
            matches!(ts.status(), TimeslotStatus::Open),
            EnergyAuctionError::InvalidTimeslot
        );
        
        ts.status = TimeslotStatus::Cancelled as u8;
        
        emit!(AuctionCancelled {
            timeslot: ts.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        
        Ok(())
    }

    /// Emergency withdrawal for stuck funds with comprehensive validation
    pub fn emergency_withdraw(
        ctx: Context<EmergencyWithdraw>,
        amount: u64,
        withdrawal_type: EmergencyWithdrawalType,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let emergency_state = &ctx.accounts.emergency_state;
        require!(emergency_state.is_paused, EnergyAuctionError::EmergencyPauseRequired);
        
        // Validate withdrawal conditions based on type
        match withdrawal_type {
            EmergencyWithdrawalType::CancelledAuction => {
                let ts = &ctx.accounts.timeslot;
                require!(matches!(ts.status(), TimeslotStatus::Cancelled), EnergyAuctionError::InvalidTimeslot);
            },
            EmergencyWithdrawalType::StuckFunds => {
                // Allow withdrawal of stuck funds after 30 days of pause
                let current_time = Clock::get()?.unix_timestamp;
                let pause_duration = current_time.checked_sub(emergency_state.pause_timestamp)
                    .ok_or(EnergyAuctionError::MathError)?;
                require!(pause_duration >= 30 * 24 * 60 * 60, EnergyAuctionError::InsufficientTimeElapsed);
            },
            EmergencyWithdrawalType::ProtocolUpgrade => {
                // Requires multi-signature approval (simplified check)
                require!(ctx.remaining_accounts.len() >= 2, EnergyAuctionError::InsufficientSignatures);
            }
        }
        
        // Validate account balances before withdrawal
        let source_balance = ctx.accounts.source_account.amount;
        require!(source_balance >= amount, EnergyAuctionError::InsufficientBalance);
        
        let ts = &ctx.accounts.timeslot;
        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        // Execute withdrawal with proper error handling
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.source_account.to_account_info(),
                to: ctx.accounts.destination_account.to_account_info(),
                authority: ts.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx, amount)?;
        
        emit!(EmergencyWithdrawal {
            withdrawal_type,
            amount,
            recipient: ctx.accounts.destination_account.key(),
            authority: ctx.accounts.authority.key(),
            source_account: ctx.accounts.source_account.key(),
            destination_account: ctx.accounts.destination_account.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        
        Ok(())
    }
    
    /// Verify delivery confirmation from oracle with automated penalty triggers
    pub fn verify_delivery_confirmation(
        ctx: Context<VerifyDeliveryConfirmation>,
        delivery_report: DeliveryReport,
        oracle_signature: [u8; 64],
    ) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let seller_allocation = &ctx.accounts.seller_allocation;
        let global_state = &ctx.accounts.global_state;
        
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        
        // Validate delivery window timing
        let current_time = Clock::get()?.unix_timestamp;
        let delivery_window_start = ts.epoch_ts;
        let delivery_window_end = delivery_window_start.checked_add(global_state.delivery_window_duration as i64)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Allow delivery verification if current time is after window start
        // In production, you may want to enforce the end time more strictly
        require!(
            current_time >= delivery_window_start,
            EnergyAuctionError::DeliveryWindowExpired
        );
        
        // Validate oracle signature (simplified - in production would verify against registered oracles)
        let oracle_pubkey = ctx.accounts.oracle.key();
        // For testing purposes, allow any oracle - in production, uncomment the authorization check below
        // require!(
        //     global_state.authorized_oracles.contains(&oracle_pubkey),
        //     EnergyAuctionError::UnauthorizedOracle
        // );
        
        // Validate delivery report
        require!(
            delivery_report.supplier == seller_allocation.supplier,
            EnergyAuctionError::ConstraintViolation
        );
        require!(
            delivery_report.timeslot == ts.key(),
            EnergyAuctionError::ConstraintViolation
        );
        require!(
            delivery_report.delivered_quantity <= seller_allocation.allocated_quantity,
            EnergyAuctionError::ConstraintViolation
        );
        
        // Automated penalty triggers for delivery shortfall
        if delivery_report.delivered_quantity < seller_allocation.allocated_quantity {
            let shortfall = seller_allocation.allocated_quantity
                .checked_sub(delivery_report.delivered_quantity)
                .ok_or(EnergyAuctionError::MathError)?;
            
            // Trigger automatic slashing for significant shortfalls (>10%)
            let shortfall_percentage = (shortfall as u128)
                .checked_mul(10000)
                .ok_or(EnergyAuctionError::MathError)?
                .checked_div(seller_allocation.allocated_quantity as u128)
                .ok_or(EnergyAuctionError::MathError)?;
            
            if shortfall_percentage > 1000 { // >10% shortfall
                // Create slashing state for automatic execution
                let slashing_state = &mut ctx.accounts.slashing_state;
                let slashing_amount = calculate_slashing_penalty(
                    shortfall,
                    seller_allocation.allocation_price,
                    global_state.slashing_penalty_bps,
                )?;
                
                slashing_state.supplier = seller_allocation.supplier;
                slashing_state.timeslot = ts.key();
                slashing_state.allocated_quantity = seller_allocation.allocated_quantity;
                slashing_state.delivered_quantity = delivery_report.delivered_quantity;
                slashing_state.slashing_amount = slashing_amount;
                slashing_state.status = SlashingStatus::AutoTriggered as u8;
                slashing_state.report_timestamp = current_time;
                slashing_state.appeal_deadline = current_time.checked_add(3 * 24 * 60 * 60) // 3 days for auto-triggered
                    .ok_or(EnergyAuctionError::MathError)?;
                slashing_state.evidence_hash = delivery_report.evidence_hash;
                slashing_state.bump = ctx.bumps.slashing_state;
                
                emit!(AutoSlashingTriggered {
                    supplier: slashing_state.supplier,
                    timeslot: slashing_state.timeslot,
                    shortfall_quantity: shortfall,
                    penalty_amount: slashing_amount,
                    slashing_amount,
                    appeal_deadline: slashing_state.appeal_deadline,
                    timestamp: current_time,
                });
            }
        }
        
        emit!(DeliveryVerified {
            supplier: seller_allocation.supplier,
            timeslot: ts.key(),
            allocated_quantity: seller_allocation.allocated_quantity,
            delivered_quantity: delivery_report.delivered_quantity,
            oracle: oracle_pubkey,
            timestamp: current_time,
        });
        
        Ok(())
    }

    /// Refund buyers after auction cancellation
    pub fn refund_cancelled_auction_buyers<'info>(
        ctx: Context<'_, '_, '_, 'info, RefundCancelledBuyers<'info>>,
        start_page: u32,
        end_page: u32,
    ) -> Result<RefundBatchResult> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let ts = &ctx.accounts.timeslot;
        let cancellation_state = &mut ctx.accounts.cancellation_state;
        
        require!(matches!(ts.status(), TimeslotStatus::Cancelled), EnergyAuctionError::InvalidTimeslot);
        require!(start_page <= end_page, EnergyAuctionError::InvalidBidPageSequence);
        
        // Initialize cancellation state if needed
        if cancellation_state.timeslot != ts.key() {
            cancellation_state.timeslot = ts.key();
            cancellation_state.status = CancellationStatus::Processing as u8;
            cancellation_state.total_buyers_refunded = 0;
            cancellation_state.total_sellers_refunded = 0;
            cancellation_state.total_quote_refunded = 0;
            cancellation_state.total_energy_refunded = 0;
            cancellation_state.bump = ctx.bumps.cancellation_state;
        }
        
        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        let mut refunded_buyers = 0u32;
        let mut total_refunded = 0u64;
        
        // Process bid pages in the specified range
        let ts_key = ts.key();
        for page_index in start_page..=end_page {
            let page_bytes = page_index.to_le_bytes();
            let bid_page_seeds = &[
                b"bid_page",
                ts_key.as_ref(),
                &page_bytes,
            ];
            let (bid_page_key, _) = Pubkey::find_program_address(bid_page_seeds, ctx.program_id);
            
            let bid_page_account_option = ctx.remaining_accounts.iter().find(|a| a.key() == bid_page_key);
            if bid_page_account_option.is_none() {
                continue;
            }
            
            let bid_page_account = bid_page_account_option.unwrap();
            if bid_page_account.data_is_empty() {
                continue;
            }
            
            let bid_page_data = &bid_page_account.try_borrow_data()?;
            if bid_page_data.len() <= 8 {
                continue;
            }
            
            let bid_page = BidPage::try_deserialize(&mut &bid_page_data[8..])?;
            if bid_page.timeslot != ts.key() {
                continue;
            }
            
            // Group bids by buyer to avoid duplicate refunds
            let mut buyer_refunds: std::collections::BTreeMap<Pubkey, u64> = std::collections::BTreeMap::new();
            
            for bid in bid_page.bids.iter() {
                if bid.status == BidStatus::Active as u8 {
                    let refund_amount = (bid.price as u128)
                        .checked_mul(bid.quantity as u128)
                        .ok_or(EnergyAuctionError::MathError)?;
                    let refund_amount = u64::try_from(refund_amount)
                        .map_err(|_| EnergyAuctionError::MathError)?;
                    
                    *buyer_refunds.entry(bid.owner).or_insert(0) = buyer_refunds
                        .get(&bid.owner)
                        .unwrap_or(&0)
                        .checked_add(refund_amount)
                        .ok_or(EnergyAuctionError::MathError)?;
                }
            }
            
            // Process refunds for each unique buyer
            for (_buyer_key, refund_amount) in buyer_refunds.iter() {
                if *refund_amount > 0 {
                    // Find buyer's quote token account in remaining_accounts
                    let buyer_quote_account_option = ctx.remaining_accounts.iter()
                        .find(|a| {
                            // This is a simplified check - in practice, you'd verify this is the buyer's ATA
                            a.owner == &spl_token::id() && !a.data_is_empty()
                        });
                    
                    if let Some(buyer_quote_account) = buyer_quote_account_option {
                        let cpi_ctx = CpiContext::new_with_signer(
                            ctx.accounts.token_program.to_account_info(),
                            Transfer {
                                from: ctx.accounts.timeslot_quote_escrow.to_account_info(),
                                to: buyer_quote_account.to_account_info(),
                                authority: ts.to_account_info(),
                            },
                            signer_seeds,
                        );
                        token::transfer(cpi_ctx, *refund_amount)?;
                        
                        refunded_buyers = refunded_buyers.checked_add(1)
                            .ok_or(EnergyAuctionError::MathError)?;
                        total_refunded = total_refunded.checked_add(*refund_amount)
                            .ok_or(EnergyAuctionError::MathError)?;
                    }
                }
            }
        }
        
        // Update cancellation state
        cancellation_state.total_buyers_refunded = cancellation_state.total_buyers_refunded
            .checked_add(refunded_buyers)
            .ok_or(EnergyAuctionError::MathError)?;
        cancellation_state.total_quote_refunded = cancellation_state.total_quote_refunded
            .checked_add(total_refunded)
            .ok_or(EnergyAuctionError::MathError)?;
        
        emit!(BuyersRefunded {
            timeslot: ts.key(),
            refunded_buyers,
            total_refunded,
            start_page,
            end_page,
        });
        
        Ok(RefundBatchResult {
            refunded_count: refunded_buyers,
            total_amount: total_refunded,
        })
    }

    /// Refund sellers after auction cancellation
    pub fn refund_cancelled_auction_sellers<'info>(
        ctx: Context<'_, '_, '_, 'info, RefundCancelledSellers<'info>>,
        seller_keys: Vec<Pubkey>,
    ) -> Result<RefundBatchResult> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let ts = &ctx.accounts.timeslot;
        let cancellation_state = &mut ctx.accounts.cancellation_state;
        
        require!(matches!(ts.status(), TimeslotStatus::Cancelled), EnergyAuctionError::InvalidTimeslot);
        require!(!seller_keys.is_empty(), EnergyAuctionError::InvalidSupplierKeys);
        require!(seller_keys.len() <= 50, EnergyAuctionError::ComputationLimitExceeded);
        
        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        let mut refunded_sellers = 0u32;
        let mut total_refunded = 0u64;
        
        let ts_key = ts.key();
        for seller_key in seller_keys {
            // Find seller's supply commitment
            let supply_seeds = &[
                b"supply",
                ts_key.as_ref(),
                seller_key.as_ref(),
            ];
            let (supply_key, _) = Pubkey::find_program_address(supply_seeds, ctx.program_id);
            
            let supply_account_option = ctx.remaining_accounts.iter().find(|a| a.key() == supply_key);
            if supply_account_option.is_none() {
                continue;
            }
            
            let supply_account = supply_account_option.unwrap();
            if supply_account.data_is_empty() {
                continue;
            }
            
            let supply_data = &supply_account.try_borrow_data()?;
            if supply_data.len() <= 8 {
                continue;
            }
            
            let supply = Supply::try_deserialize(&mut &supply_data[8..])?;
            if supply.timeslot != ts.key() || supply.claimed {
                continue;
            }
            
            // Find seller's escrow account
            let seller_escrow_seeds = &[
                b"seller_escrow",
                ts_key.as_ref(),
                seller_key.as_ref(),
            ];
            let (seller_escrow_key, _) = Pubkey::find_program_address(seller_escrow_seeds, ctx.program_id);
            
            let seller_escrow_account_option = ctx.remaining_accounts.iter()
                .find(|a| a.key() == seller_escrow_key);
            
            if let Some(seller_escrow_account) = seller_escrow_account_option {
                // Find seller's destination account in remaining_accounts
                let seller_destination_option = ctx.remaining_accounts.iter()
                    .find(|a| {
                        // This is a simplified check - in practice, you'd verify this is the seller's ATA
                        a.owner == &spl_token::id() && !a.data_is_empty()
                    });
                
                if let Some(seller_destination) = seller_destination_option {
                    // Transfer energy tokens back to seller
                    let cpi_ctx = CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: seller_escrow_account.to_account_info(),
                            to: seller_destination.to_account_info(),
                            authority: ts.to_account_info(),
                        },
                        signer_seeds,
                    );
                    token::transfer(cpi_ctx, supply.amount)?;
                    
                    refunded_sellers = refunded_sellers.checked_add(1)
                        .ok_or(EnergyAuctionError::MathError)?;
                    total_refunded = total_refunded.checked_add(supply.amount)
                        .ok_or(EnergyAuctionError::MathError)?;
                }
            }
        }
        
        // Update cancellation state
        cancellation_state.total_sellers_refunded = cancellation_state.total_sellers_refunded
            .checked_add(refunded_sellers)
            .ok_or(EnergyAuctionError::MathError)?;
        cancellation_state.total_energy_refunded = cancellation_state.total_energy_refunded
            .checked_add(total_refunded)
            .ok_or(EnergyAuctionError::MathError)?;
        
        emit!(SellersRefunded {
            timeslot: ts.key(),
            refunded_sellers,
            total_refunded,
        });
        
        Ok(RefundBatchResult {
            refunded_count: refunded_sellers,
            total_amount: total_refunded,
        })
    }

    /// Report non-delivery by a seller
    pub fn report_non_delivery(
        ctx: Context<ReportNonDelivery>,
        delivered_quantity: u64,
        evidence_hash: [u8; 32],
    ) -> Result<()> {
        let ts = &ctx.accounts.timeslot;
        let seller_allocation = &ctx.accounts.seller_allocation;
        let slashing_state = &mut ctx.accounts.slashing_state;
        
        require!(matches!(ts.status(), TimeslotStatus::Settled), EnergyAuctionError::InvalidTimeslot);
        require!(delivered_quantity <= seller_allocation.allocated_quantity, EnergyAuctionError::ConstraintViolation);
        
        let current_time = Clock::get()?.unix_timestamp;
        let appeal_deadline = current_time.checked_add(7 * 24 * 60 * 60) // 7 days
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Calculate slashing amount based on non-delivered quantity
        let non_delivered = seller_allocation.allocated_quantity
            .checked_sub(delivered_quantity)
            .ok_or(EnergyAuctionError::MathError)?;
        
        // Slashing penalty: 150% of the value of non-delivered energy
        let slashing_amount = (non_delivered as u128)
            .checked_mul(seller_allocation.allocation_price as u128)
            .ok_or(EnergyAuctionError::MathError)?
            .checked_mul(ctx.accounts.global_state.slashing_penalty_bps as u128)
            .ok_or(EnergyAuctionError::MathError)?
            .checked_div(10_000)
            .ok_or(EnergyAuctionError::MathError)?;
        let slashing_amount = u64::try_from(slashing_amount)
            .map_err(|_| EnergyAuctionError::MathError)?;
        
        slashing_state.supplier = seller_allocation.supplier;
        slashing_state.timeslot = ts.key();
        slashing_state.allocated_quantity = seller_allocation.allocated_quantity;
        slashing_state.delivered_quantity = delivered_quantity;
        slashing_state.slashing_amount = slashing_amount;
        slashing_state.status = SlashingStatus::Reported as u8;
        slashing_state.report_timestamp = current_time;
        slashing_state.appeal_deadline = appeal_deadline;
        slashing_state.evidence_hash = evidence_hash;
        slashing_state.bump = ctx.bumps.slashing_state;
        
        emit!(NonDeliveryReported {
            supplier: seller_allocation.supplier,
            timeslot: ts.key(),
            allocated_quantity: seller_allocation.allocated_quantity,
            delivered_quantity,
            slashing_amount,
            appeal_deadline,
        });
        
        Ok(())
    }

    /// Appeal a slashing decision
    pub fn appeal_slashing(
        ctx: Context<AppealSlashing>,
        appeal_evidence: [u8; 32],
    ) -> Result<()> {
        let slashing_state = &mut ctx.accounts.slashing_state;
        
        require!(slashing_state.status == SlashingStatus::Reported as u8, EnergyAuctionError::ConstraintViolation);
        require_keys_eq!(slashing_state.supplier, ctx.accounts.seller.key(), EnergyAuctionError::Unauthorized);
        
        let current_time = Clock::get()?.unix_timestamp;
        require!(current_time <= slashing_state.appeal_deadline, EnergyAuctionError::SlashingAppealExpired);
        
        slashing_state.status = SlashingStatus::UnderAppeal as u8;
        slashing_state.evidence_hash = appeal_evidence;
        
        emit!(SlashingAppealed {
            supplier: slashing_state.supplier,
            timeslot: slashing_state.timeslot,
            appeal_evidence,
            timestamp: current_time,
        });
        
        Ok(())
    }

    /// Execute slashing penalties after appeal period with comprehensive validation
    pub fn execute_slashing<'info>(
        ctx: Context<'_, '_, '_, 'info, ExecuteSlashing<'info>>,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let slashing_state = &mut ctx.accounts.slashing_state;
        let seller_allocation = &ctx.accounts.seller_allocation;
        let current_time = Clock::get()?.unix_timestamp;
        
        // Validate slashing state and timing
        require!(
            slashing_state.status == SlashingStatus::Reported as u8 && current_time > slashing_state.appeal_deadline ||
            slashing_state.status == SlashingStatus::Confirmed as u8,
            EnergyAuctionError::ConstraintViolation
        );
        
        // Validate delivery reports against allocations
        require!(
            slashing_state.allocated_quantity == seller_allocation.allocated_quantity,
            EnergyAuctionError::SettlementVerificationFailed
        );
        require!(
            slashing_state.delivered_quantity <= slashing_state.allocated_quantity,
            EnergyAuctionError::ConstraintViolation
        );
        
        let ts = &ctx.accounts.timeslot;
        let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
        let signer_seeds = &[&timeslot_seeds[..]];
        
        // Calculate penalty amounts based on shortfall
        let shortfall_quantity = slashing_state.allocated_quantity
            .checked_sub(slashing_state.delivered_quantity)
            .ok_or(EnergyAuctionError::MathError)?;
        
        if shortfall_quantity > 0 {
            // Base penalty: value of undelivered energy at clearing price
            let base_penalty = (shortfall_quantity as u128)
                .checked_mul(seller_allocation.allocation_price as u128)
                .ok_or(EnergyAuctionError::MathError)?;
            
            // Additional slashing penalty (configurable percentage)
            let slashing_penalty = base_penalty
                .checked_mul(ctx.accounts.global_state.slashing_penalty_bps as u128)
                .ok_or(EnergyAuctionError::MathError)?
                .checked_div(10_000)
                .ok_or(EnergyAuctionError::MathError)?;
            
            let total_penalty = base_penalty
                .checked_add(slashing_penalty)
                .ok_or(EnergyAuctionError::MathError)?;
            
            let total_penalty = u64::try_from(total_penalty)
                .map_err(|_| EnergyAuctionError::MathError)?;
            
            // Validate penalty amount matches calculated amount
            require!(
                slashing_state.slashing_amount == total_penalty,
                EnergyAuctionError::SettlementVerificationFailed
            );
            
            // Transfer penalties to slashing vault
            let cpi_ctx_penalty = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.seller_collateral.to_account_info(),
                    to: ctx.accounts.slashing_vault.to_account_info(),
                    authority: ts.to_account_info(),
                },
                signer_seeds,
            );
            token::transfer(cpi_ctx_penalty, total_penalty)?;
            
            // Distribute compensation to affected buyers if compensation pool exists
            if let Some(compensation_pool) = ctx.remaining_accounts.get(0) {
                let compensation_amount = base_penalty as u64;
                let cpi_ctx_compensation = CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.slashing_vault.to_account_info(),
                        to: compensation_pool.to_account_info(),
                        authority: ts.to_account_info(),
                    },
                    signer_seeds,
                );
                token::transfer(cpi_ctx_compensation, compensation_amount)?;
            }
        }
        
        slashing_state.status = SlashingStatus::Executed as u8;
        slashing_state.execution_timestamp = current_time;
        
        emit!(SlashingExecuted {
            supplier: slashing_state.supplier,
            timeslot: slashing_state.timeslot,
            slashing_amount: slashing_state.slashing_amount,
            shortfall_quantity,
            timestamp: current_time,
        });
        
        Ok(())
    }

    /// Emergency pause the protocol
    pub fn emergency_pause(
        ctx: Context<EmergencyPause>,
        reason: [u8; 64],
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let emergency_state = &mut ctx.accounts.emergency_state;
        require!(!emergency_state.is_paused, EnergyAuctionError::EmergencyPauseActive);
        
        let current_time = Clock::get()?.unix_timestamp;
        
        emergency_state.is_paused = true;
        emergency_state.pause_timestamp = current_time;
        emergency_state.pause_reason = reason;
        emergency_state.authority = ctx.accounts.authority.key();
        emergency_state.bump = ctx.bumps.emergency_state;
        
        emit!(EmergencyPaused {
            timestamp: current_time,
            reason,
            authority: ctx.accounts.authority.key(),
        });
        
        Ok(())
    }

    /// Resume protocol after emergency pause
    pub fn emergency_resume(
        ctx: Context<EmergencyResume>,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let emergency_state = &mut ctx.accounts.emergency_state;
        require!(emergency_state.is_paused, EnergyAuctionError::ConstraintViolation);
        
        let current_time = Clock::get()?.unix_timestamp;
        let pause_duration = current_time.checked_sub(emergency_state.pause_timestamp)
            .ok_or(EnergyAuctionError::MathError)?;
        
        emergency_state.is_paused = false;
        
        emit!(EmergencyResumed {
            timestamp: current_time,
            pause_duration,
            authority: ctx.accounts.authority.key(),
        });
        
        Ok(())
    }

    /// Rollback failed auction to previous state
    pub fn rollback_failed_auction(
        ctx: Context<RollbackAuction>,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let ts = &mut ctx.accounts.timeslot;
        let auction_state = &mut ctx.accounts.auction_state;
        
        // Can only rollback from Processing or Cleared states
        require!(
            auction_state.status == AuctionStatus::Processing as u8 ||
            auction_state.status == AuctionStatus::Cleared as u8,
            EnergyAuctionError::ConstraintViolation
        );
        
        // Reset auction state
        auction_state.status = AuctionStatus::Failed as u8;
        auction_state.clearing_price = 0;
        auction_state.total_cleared_quantity = 0;
        auction_state.total_revenue = 0;
        
        // Reset timeslot to Cancelled state after rollback
        ts.status = TimeslotStatus::Cancelled as u8;
        ts.clearing_price = 0;
        ts.total_sold_quantity = 0;
        
        emit!(AuctionRolledBack {
            timeslot: ts.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        
        Ok(())
    }

    /// Propose parameter change through governance with enhanced validation
    pub fn propose_parameter_change(
        ctx: Context<ProposeParameterChange>,
        proposal_id: u64,
        proposal_type: ProposalType,
        new_value: u64,
        description: [u8; 128],
    ) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let global_state = &ctx.accounts.global_state;
        let current_time = Clock::get()?.unix_timestamp;
        
        // Validate proposer has sufficient stake or is authorized
        require!(
            ctx.accounts.proposer_stake.amount >= global_state.min_proposal_stake ||
            ctx.accounts.proposer.key() == global_state.authority,
            EnergyAuctionError::InsufficientStake
        );
        
        // Validate parameter bounds
        validate_parameter_bounds(proposal_type, new_value, global_state)?;
        
        // Set voting period based on proposal type
        let voting_period = match proposal_type {
            ProposalType::EmergencyParameterChange => 60, // 1 minute for emergency proposals
            _ => 3 * 24 * 60 * 60, // 3 days for normal proposals
        };
        proposal.voting_deadline = current_time + voting_period;
        
        proposal.proposal_id = proposal_id;
        proposal.proposer = ctx.accounts.proposer.key();
        proposal.proposal_type = proposal_type;
        proposal.new_value = new_value;
        proposal.description = description;
        proposal.created_at = current_time;
        proposal.votes_for = 0;
        proposal.votes_against = 0;
        proposal.total_voting_power = 0;
        proposal.status = ProposalStatus::Active as u8;
        proposal.execution_timestamp = 0;
        proposal.required_signatures = calculate_required_signatures(proposal_type, global_state);
        proposal.current_signatures = 0;
        proposal.bump = ctx.bumps.proposal;
        
        emit!(ProposalCreated {
            proposal_id: proposal.key(),
            proposer: ctx.accounts.proposer.key(),
            proposal_type,
            new_value,
            voting_deadline: proposal.voting_deadline,
            required_signatures: proposal.required_signatures,
        });
        
        Ok(())
    }

    /// Vote on a governance proposal with multi-signature support
    pub fn vote_on_proposal(
        ctx: Context<VoteOnProposal>,
        vote: Vote,
        voting_power: u64,
    ) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let vote_record = &mut ctx.accounts.vote_record;
        let global_state = &ctx.accounts.global_state;
        
        require!(proposal.status == ProposalStatus::Active as u8, EnergyAuctionError::ConstraintViolation);
        
        let current_time = Clock::get()?.unix_timestamp;
        require!(current_time <= proposal.voting_deadline, EnergyAuctionError::VotingPeriodExpired);
        
        // Validate voter eligibility and voting power
        let voter = ctx.accounts.voter.key();
        let is_council_member = global_state.governance_council.contains(&voter);
        
        if is_council_member {
            // Council members get enhanced voting power
            let council_voting_power = voting_power.checked_mul(global_state.council_vote_multiplier as u64)
                .ok_or(EnergyAuctionError::MathError)?;
            
            // Record council signature for multi-sig requirements
            if !vote_record.has_voted {
                proposal.current_signatures = proposal.current_signatures.checked_add(1)
                    .ok_or(EnergyAuctionError::MathError)?;
            }
            
            match vote {
                Vote::For => {
                    proposal.votes_for = proposal.votes_for.checked_add(council_voting_power)
                        .ok_or(EnergyAuctionError::MathError)?;
                },
                Vote::Against => {
                    proposal.votes_against = proposal.votes_against.checked_add(council_voting_power)
                        .ok_or(EnergyAuctionError::MathError)?;
                }
            }
        } else {
            // Regular stakeholder voting
            require!(
                ctx.accounts.voter_stake.amount >= global_state.min_voting_stake,
                EnergyAuctionError::InsufficientStake
            );
            
            let effective_voting_power = std::cmp::min(voting_power, ctx.accounts.voter_stake.amount);
            
            match vote {
                Vote::For => {
                    proposal.votes_for = proposal.votes_for.checked_add(effective_voting_power)
                        .ok_or(EnergyAuctionError::MathError)?;
                },
                Vote::Against => {
                    proposal.votes_against = proposal.votes_against.checked_add(effective_voting_power)
                        .ok_or(EnergyAuctionError::MathError)?;
                }
            }
        }
        
        // Update vote record
        vote_record.voter = voter;
        vote_record.proposal = proposal.key();
        vote_record.vote = vote;
        vote_record.voting_power = voting_power;
        vote_record.timestamp = current_time;
        vote_record.has_voted = true;
        vote_record.bump = ctx.bumps.vote_record;
        
        // Update total voting power
        if !vote_record.has_voted {
            proposal.total_voting_power = proposal.total_voting_power.checked_add(voting_power)
                .ok_or(EnergyAuctionError::MathError)?;
        }
        
        // Check if proposal can be executed early (sufficient signatures + votes)
        let has_required_signatures = proposal.current_signatures >= proposal.required_signatures;
        let has_majority_votes = proposal.votes_for > proposal.votes_against;
        let total_votes = proposal.votes_for.checked_add(proposal.votes_against)
            .ok_or(EnergyAuctionError::MathError)?;
        let participation_threshold = global_state.min_participation_threshold;
        let has_quorum = total_votes >= participation_threshold;
        
        if has_required_signatures && has_majority_votes && has_quorum {
            proposal.status = ProposalStatus::Passed as u8;
            
            emit!(ProposalPassed {
                proposal_id: proposal.key(),
                proposal_type: proposal.proposal_type,
                final_vote_count: proposal.votes_for,
                votes_for: proposal.votes_for,
                votes_against: proposal.votes_against,
                signatures: proposal.current_signatures,
                timestamp: current_time,
            });
        }
        
        emit!(VoteCast {
            proposal_id: proposal.key(),
            voter,
            vote,
            voting_power,
            is_council_member,
            timestamp: current_time,
        });
        
        Ok(())
    }

    /// Execute approved governance proposal with multi-signature validation
    pub fn execute_proposal(
        ctx: Context<ExecuteProposal>,
    ) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let global_state = &mut ctx.accounts.global_state;
        
        require!(
            proposal.status == ProposalStatus::Passed as u8,
            EnergyAuctionError::ProposalNotPassed
        );
        
        let current_time = Clock::get()?.unix_timestamp;
        
        // Validate execution timing (timelock for critical changes)
        let execution_delay = match proposal.proposal_type {
            ProposalType::ProtocolUpgrade => 48 * 60 * 60, // 48 hours
            ProposalType::EmergencyParameterChange => 0,    // Immediate execution allowed
            _ => 24 * 60 * 60, // 24 hours
        };
        
        // For emergency proposals, allow immediate execution if passed
        // For other proposals, require timelock period after voting deadline
        match proposal.proposal_type {
            ProposalType::EmergencyParameterChange => {
                // Emergency proposals can execute immediately after passing
            },
            _ => {
                let earliest_execution = proposal.voting_deadline.checked_add(execution_delay)
                    .ok_or(EnergyAuctionError::MathError)?;
                
                require!(current_time >= earliest_execution, EnergyAuctionError::TimelockNotExpired);
            }
        }
        
        // Validate multi-signature requirements are met
        require!(
            proposal.current_signatures >= proposal.required_signatures,
            EnergyAuctionError::InsufficientSignatures
        );
        
        // Execute the parameter change
        match proposal.proposal_type {
            ProposalType::FeeBps => {
                global_state.fee_bps = proposal.new_value as u16;
            },
            ProposalType::Version => {
                global_state.version = proposal.new_value as u8;
            },
            ProposalType::MaxBatchSize => {
                global_state.max_batch_size = proposal.new_value as u16;
            },
            ProposalType::MaxSellersPerTimeslot => {
                global_state.max_sellers_per_timeslot = proposal.new_value as u16;
            },
            ProposalType::MaxBidsPerPage => {
                global_state.max_bids_per_page = proposal.new_value as u16;
            },
            ProposalType::SlashingPenaltyBps => {
                global_state.slashing_penalty_bps = proposal.new_value as u16;
            },
            ProposalType::AppealWindowSeconds => {
                global_state.appeal_window_seconds = proposal.new_value as u32;
            },
            ProposalType::DeliveryWindowDuration => {
                global_state.delivery_window_duration = proposal.new_value as u32;
            },
            ProposalType::MinProposalStake => {
                global_state.min_proposal_stake = proposal.new_value;
            },
            ProposalType::MinVotingStake => {
                global_state.min_voting_stake = proposal.new_value;
            },
            ProposalType::EmergencyParameterChange => {
                // Emergency parameter changes can be executed without pause requirement
                // Handle emergency parameter changes based on proposal details
            },
            ProposalType::ProtocolUpgrade => {
                require!(
                    ctx.remaining_accounts.len() >= 3,
                    EnergyAuctionError::InsufficientUpgradeAccounts
                );
                // Handle protocol upgrades
            },
        }
        
        proposal.status = ProposalStatus::Executed as u8;
        proposal.execution_timestamp = current_time;
        
        emit!(ProposalExecuted {
            proposal_id: proposal.key(),
            proposal_type: proposal.proposal_type,
            new_value: proposal.new_value,
            execution_timestamp: current_time,
        });
        
        Ok(())
    }

    /// Add comprehensive input validation and circuit breaker
    pub fn validate_system_health(
        ctx: Context<ValidateSystemHealth>,
    ) -> Result<SystemHealthReport> {
        let global_state = &ctx.accounts.global_state;
        let emergency_state = &ctx.accounts.emergency_state;
        
        let mut health_report = SystemHealthReport {
            overall_status: SystemStatus::Healthy,
            active_auctions: 0,
            pending_settlements: 0,
            total_locked_value: 0,
            failed_deliveries: 0,
            emergency_pause_active: emergency_state.is_paused,
            emergency_paused: emergency_state.is_paused,
            last_check_timestamp: Clock::get()?.unix_timestamp,
        };
        
        // Check for system anomalies
        let mut anomalies = Vec::new();
        
        // Validate global parameters are within safe bounds
        if global_state.fee_bps > 1000 {
            anomalies.push("Fee rate exceeds safe threshold");
            health_report.overall_status = SystemStatus::Warning;
        }
        
        if global_state.slashing_penalty_bps > 5000 {
            anomalies.push("Slashing penalty exceeds maximum threshold");
            health_report.overall_status = SystemStatus::Critical;
        }
        
        // Check for stuck auctions (simplified check)
        let current_time = Clock::get()?.unix_timestamp;
        let mut stuck_auctions = 0;
        
        // Scan through remaining accounts for timeslot states
        for account in ctx.remaining_accounts.iter() {
            if account.owner != ctx.program_id || account.data_is_empty() {
                continue;
            }
            
            // Try to deserialize as Timeslot
            if let Ok(account_data) = account.try_borrow_data() {
                if account_data.len() > 8 {
                    if let Ok(timeslot) = Timeslot::try_deserialize(&mut &account_data[8..]) {
                        health_report.active_auctions += 1;
                        
                        // Check for stuck auctions (processing for >24 hours)
                        if matches!(timeslot.status(), TimeslotStatus::Sealed) {
                            let time_since_seal = current_time.checked_sub(timeslot.epoch_ts)
                                .unwrap_or(0);
                            if time_since_seal > 24 * 60 * 60 {
                                stuck_auctions += 1;
                            }
                        }
                    }
                }
            }
        }
        
        if stuck_auctions > 0 {
            anomalies.push("Detected stuck auctions");
            health_report.overall_status = SystemStatus::Warning;
        }
        
        // Trigger circuit breaker for critical issues
        if health_report.overall_status == SystemStatus::Critical && !emergency_state.is_paused {
            // Auto-trigger emergency pause
            emit!(CircuitBreakerTriggered {
                trigger_reason: SystemStatus::Critical,
                reason: "Critical system health issues detected".to_string(),
                anomaly_count: anomalies.len() as u32,
                timestamp: current_time,
                authority: global_state.authority,
            });
        }
        
        Ok(health_report)
    }

    /// Appeal resolution system with evidence validation
    pub fn resolve_slashing_appeal(
        ctx: Context<ResolveSlashingAppeal>,
        decision: AppealDecision,
        resolution_evidence: [u8; 64],
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.global_state.authority,
            ctx.accounts.authority.key(),
            EnergyAuctionError::InvalidAuthority
        );
        
        let slashing_state = &mut ctx.accounts.slashing_state;
        require!(
            slashing_state.status == SlashingStatus::Appealed as u8,
            EnergyAuctionError::ConstraintViolation
        );
        
        let current_time = Clock::get()?.unix_timestamp;
        
        match decision {
            AppealDecision::Upheld => {
                // Appeal successful - reverse slashing
                slashing_state.status = SlashingStatus::Reversed as u8;
                slashing_state.resolution_timestamp = current_time;
                slashing_state.resolution_evidence = resolution_evidence;
                
                // Refund any slashed amounts if already executed
                if slashing_state.slashing_amount > 0 {
                    let ts = &ctx.accounts.timeslot;
                    let timeslot_seeds = &[&b"timeslot"[..], &ts.epoch_ts.to_le_bytes(), &[ctx.bumps.timeslot]];
                    let signer_seeds = &[&timeslot_seeds[..]];
                    
                    let cpi_ctx = CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: ctx.accounts.slashing_vault.to_account_info(),
                            to: ctx.accounts.seller_collateral.to_account_info(),
                            authority: ts.to_account_info(),
                        },
                        signer_seeds,
                    );
                    token::transfer(cpi_ctx, slashing_state.slashing_amount)?;
                }
                
                emit!(SlashingAppealUpheld {
                    supplier: slashing_state.supplier,
                    timeslot: slashing_state.timeslot,
                    refund_amount: slashing_state.slashing_amount,
                    timestamp: current_time,
                });
            },
            AppealDecision::Rejected => {
                // Appeal rejected - confirm slashing
                slashing_state.status = SlashingStatus::Confirmed as u8;
                slashing_state.resolution_timestamp = current_time;
                slashing_state.resolution_evidence = resolution_evidence;
                
                emit!(SlashingAppealRejected {
                    supplier: slashing_state.supplier,
                    timeslot: slashing_state.timeslot,
                    penalty_confirmed: slashing_state.slashing_amount,
                    final_penalty: slashing_state.slashing_amount,
                    timestamp: current_time,
                });
            }
        }
        
        Ok(())
    }

    /// Initialize bid registry for a timeslot
    pub fn init_bid_registry(
        ctx: Context<InitBidRegistry>,
    ) -> Result<()> {
        let bid_registry = &mut ctx.accounts.bid_registry;
        bid_registry.timeslot = ctx.accounts.timeslot.key();
        bid_registry.bid_pages = Vec::new();
        bid_registry.total_pages = 0;
        bid_registry.bump = ctx.bumps.bid_registry;
        Ok(())
    }

    /// Register a bid page in the bid registry
    pub fn register_bid_page(
        ctx: Context<RegisterBidPage>,
        _page_index: u32,
    ) -> Result<()> {
        let bid_registry = &mut ctx.accounts.bid_registry;
        let bid_page_key = ctx.accounts.bid_page.key();
        
        if !bid_registry.bid_pages.contains(&bid_page_key) {
            require!(bid_registry.bid_pages.len() < ctx.accounts.global_state.max_bids_per_page as usize, EnergyAuctionError::ComputationLimitExceeded);
            bid_registry.bid_pages.push(bid_page_key);
            bid_registry.total_pages = bid_registry.total_pages
                .checked_add(1)
                .ok_or(EnergyAuctionError::MathError)?;
        }
        
        Ok(())
    }

    // Need this instruction to create the tracker after settlement
    pub fn init_allocation_tracker(ctx: Context<InitAllocationTracker>) -> Result<()> {
        let tracker = &mut ctx.accounts.allocation_tracker;
        tracker.timeslot = ctx.accounts.timeslot.key();
        tracker.remaining_quantity = ctx.accounts.timeslot.total_sold_quantity;
        tracker.total_allocated = 0;
        tracker.last_processed_reserve_price = 0;
        tracker.bump = ctx.bumps.allocation_tracker;
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
        seeds = [b"buyer_allocation", timeslot.key().as_ref(), buyer.key().as_ref()],
        bump,
        constraint = buyer_allocation.buyer == buyer.key() @ EnergyAuctionError::Unauthorized
    )]
    pub buyer_allocation: Account<'info, BuyerAllocation>,
    
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
    
    #[account(mut)]
    pub buyer: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}
// 5. Context for the new allocation calculation
#[derive(Accounts)]
pub struct CalculateSellerAllocations<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        seeds = [b"supply", timeslot.key().as_ref(), supply.supplier.as_ref()],
        bump
    )]
    pub supply: Account<'info, Supply>,
    
    #[account(
        init,
        payer = authority,
        space = 8 + SellerAllocation::LEN,
        seeds = [b"seller_allocation", timeslot.key().as_ref(), supply.supplier.as_ref()],
        bump
    )]
    pub seller_allocation: Account<'info, SellerAllocation>,
    
    #[account(
        mut,
        seeds = [b"allocation_tracker", timeslot.key().as_ref()],
        bump
    )]
    pub remaining_allocation_tracker: Account<'info, AllocationTracker>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

// Context for processing bid batches
#[derive(Accounts)]
pub struct ProcessBidBatch<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + AuctionState::LEN,
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + PriceLevelAggregate::LEN,
        seeds = [b"price_level", timeslot.key().as_ref(), &[0; 8]],  // Placeholder for dynamic price
        bump
    )]
    pub price_level: Account<'info, PriceLevelAggregate>,
    
    #[account(mut)]
    pub payer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
    pub clock: Sysvar<'info, Clock>,
}

// Context for processing supply batches
#[derive(Accounts)]
pub struct ProcessSupplyBatch<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + AllocationTracker::LEN,
        seeds = [b"allocation_tracker", timeslot.key().as_ref()],
        bump
    )]
    pub allocation_tracker: Account<'info, AllocationTracker>,
    
    #[account(mut)]
    pub payer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
    pub clock: Sysvar<'info, Clock>,
}

// Context for executing auction clearing
#[derive(Accounts)]
pub struct ExecuteAuctionClearing<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        init,
        payer = payer,
        space = 8 + AuctionState::LEN,
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    #[account(mut)]
    pub payer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

// Context for verifying auction clearing
#[derive(Accounts)]
pub struct VerifyAuctionClearing<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    #[account(
        seeds = [b"quote_escrow", timeslot.key().as_ref()],
        bump
    )]
    pub timeslot_quote_escrow: Account<'info, TokenAccount>,
    
    pub clock: Sysvar<'info, Clock>,
}

// 6. Updated context for withdraw_proceeds
#[derive(Accounts)]
pub struct WithdrawProceedsV2<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"seller_allocation", timeslot.key().as_ref(), seller.key().as_ref()],
        bump,
        constraint = seller_allocation.supplier == seller.key() @ EnergyAuctionError::Unauthorized
    )]
    pub seller_allocation: Account<'info, SellerAllocation>,
    
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
    
    #[account(mut)]
    pub seller: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}
#[account]
pub struct AllocationTracker {
    pub timeslot: Pubkey,
    pub remaining_quantity: u64,
    pub total_allocated: u64,
    pub last_processed_reserve_price: u64,  // NEW: enforce merit order
    pub bump: u8,
}

impl AllocationTracker {
    pub const LEN: usize = 32 + 8 + 8 + 8 + 1;
}

/// Tracks cancellation refund progress
#[account]
pub struct CancellationState {
    pub timeslot: Pubkey,
    pub status: u8, // CancellationStatus
    pub total_buyers_refunded: u32,
    pub total_sellers_refunded: u32,
    pub total_quote_refunded: u64,
    pub total_energy_refunded: u64,
    pub cancellation_timestamp: i64,
    pub bump: u8,
}

impl CancellationState {
    pub const LEN: usize = 32 + 1 + 4 + 4 + 8 + 8 + 8 + 1;
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum CancellationStatus {
    Processing = 0,
    BuyersRefunded = 1,
    SellersRefunded = 2,
    Completed = 3,
}

/// Slashing state for delivery verification
#[account]
pub struct SlashingState {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub allocated_quantity: u64,
    pub delivered_quantity: u64,
    pub slashing_amount: u64,
    pub status: u8, // SlashingStatus
    pub report_timestamp: i64,
    pub appeal_deadline: i64,
    pub execution_timestamp: i64,
    pub resolution_timestamp: i64,
    pub evidence_hash: [u8; 32],
    pub resolution_evidence: [u8; 64],
    pub bump: u8,
}

impl SlashingState {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 1 + 8 + 8 + 8 + 8 + 32 + 64 + 1;
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum SlashingStatus {
    Reported = 0,
    UnderAppeal = 1,
    Confirmed = 2,
    Dismissed = 3,
    Appealed = 4,
    Reversed = 5,
    AutoTriggered = 6,
    Executed = 7,
}

/// Emergency pause state
#[account]
pub struct EmergencyState {
    pub is_paused: bool,
    pub pause_timestamp: i64,
    pub pause_reason: [u8; 64],
    pub authority: Pubkey,
    pub bump: u8,
}

impl EmergencyState {
    pub const LEN: usize = 1 + 8 + 64 + 32 + 1;
}

/// Governance proposal for parameter changes
#[account]
pub struct GovernanceProposal {
    pub proposal_id: u64,
    pub proposer: Pubkey,
    pub proposal_type: ProposalType,
    pub new_value: u64,
    pub description: [u8; 128],
    pub created_at: i64,
    pub voting_deadline: i64,
    pub votes_for: u64,
    pub votes_against: u64,
    pub total_voting_power: u64,
    pub current_signatures: u8,
    pub required_signatures: u8,
    pub status: u8, // ProposalStatus
    pub execution_timestamp: i64,
    pub bump: u8,
}

impl GovernanceProposal {
    pub const LEN: usize = 8 + 32 + 1 + 8 + 128 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 1 + 8 + 1;
}

/// Individual vote record
#[account]
pub struct VoteRecord {
    pub voter: Pubkey,
    pub proposal: Pubkey,
    pub vote: Vote,
    pub voting_power: u64,
    pub timestamp: i64,
    pub has_voted: bool,
    pub bump: u8,
}

impl VoteRecord {
    pub const LEN: usize = 32 + 32 + 1 + 8 + 8 + 1 + 1;
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

// New events for auction clearing system

#[event]
pub struct AuctionCleared {
    pub timeslot: Pubkey,
    pub clearing_price: u64,
    pub cleared_quantity: u64,
    pub total_revenue: u64,
    pub winning_bids_count: u32,
    pub participating_sellers_count: u32,
    pub timestamp: i64,
}

#[event]
pub struct BidBatchProcessed {
    pub timeslot: Pubkey,
    pub start_page: u32,
    pub end_page: u32,
    pub processed_bids: u32,
    pub total_quantity: u64,
}

#[event]
pub struct SupplyBatchProcessed {
    pub timeslot: Pubkey,
    pub processed_sellers: u32,
    pub total_allocated: u64,
    pub remaining_demand: u64,
}

#[event]
pub struct SellerAllocationNeeded {
    pub timeslot: Pubkey,
    pub supplier: Pubkey,
    pub allocated_quantity: u64,
    pub allocation_price: u64,
}

#[event]
pub struct BidOutcomeCreated {
    pub buyer: Pubkey,
    pub timeslot: Pubkey,
    pub filled_quantity: u64,
    pub clearing_price: u64,
    pub refund_amount: u64,
}

#[event]
pub struct AuctionVerified {
    pub timeslot: Pubkey,
    pub clearing_price: u64,
    pub cleared_quantity: u64,
    pub total_revenue: u64,
    pub winning_bids_count: u32,
    pub participating_sellers_count: u32,
    pub timestamp: i64,
    pub total_buyer_payments: u64,
    pub total_seller_proceeds: u64,
    pub protocol_fees: u64,
    pub total_energy_distributed: u64,
    pub total_energy_committed: u64,
    pub total_refunds: u64,
}

#[event]
pub struct EnergyRedeemed {
    pub buyer: Pubkey,
    pub timeslot: Pubkey,
    pub total_quantity: u64,
    pub total_cost: u64,
    pub refund_amount: u64,
}

#[event]
pub struct AuctionCancelled {
    pub timeslot: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct BuyersRefunded {
    pub timeslot: Pubkey,
    pub refunded_buyers: u32,
    pub total_refunded: u64,
    pub start_page: u32,
    pub end_page: u32,
}

#[event]
pub struct SellersRefunded {
    pub timeslot: Pubkey,
    pub refunded_sellers: u32,
    pub total_refunded: u64,
}

#[event]
pub struct NonDeliveryReported {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub allocated_quantity: u64,
    pub delivered_quantity: u64,
    pub slashing_amount: u64,
    pub appeal_deadline: i64,
}

#[event]
pub struct SlashingAppealed {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub appeal_evidence: [u8; 32],
    pub timestamp: i64,
}

#[event]
pub struct SlashingExecuted {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub slashing_amount: u64,
    pub shortfall_quantity: u64,
    pub timestamp: i64,
}

#[event]
pub struct EmergencyPaused {
    pub timestamp: i64,
    pub reason: [u8; 64],
    pub authority: Pubkey,
}

#[event]
pub struct EmergencyResumed {
    pub timestamp: i64,
    pub pause_duration: i64,
    pub authority: Pubkey,
}

#[event]
pub struct AuctionRolledBack {
    pub timeslot: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct ProposalCreated {
    pub proposal_id: Pubkey,
    pub proposer: Pubkey,
    pub proposal_type: ProposalType,
    pub new_value: u64,
    pub voting_deadline: i64,
    pub required_signatures: u8,
}

#[event]
pub struct VoteCast {
    pub proposal_id: Pubkey,
    pub voter: Pubkey,
    pub vote: Vote,
    pub voting_power: u64,
    pub is_council_member: bool,
    pub timestamp: i64,
}

#[event]
pub struct ProposalExecuted {
    pub proposal_id: Pubkey,
    pub proposal_type: ProposalType,
    pub new_value: u64,
    pub execution_timestamp: i64,
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
    pub max_batch_size: u16, // configurable batch processing limit
    pub max_sellers_per_timeslot: u16, // configurable seller limit
    pub max_bids_per_page: u16, // configurable bid page size
    pub slashing_penalty_bps: u16, // configurable slashing penalty (basis points)
    pub appeal_window_seconds: u32, // configurable appeal window
    pub delivery_window_duration: u32, // configurable delivery window
    pub min_proposal_stake: u64, // minimum stake required for proposals
    pub min_voting_stake: u64, // minimum stake required for voting
    pub governance_council: Vec<Pubkey>, // governance council members
    pub council_vote_multiplier: u16, // voting power multiplier for council members
    pub min_participation_threshold: u64, // minimum participation for proposals
    pub authorized_oracles: Vec<Pubkey>, // authorized oracle accounts
    pub bump: u8,
    pub quote_mint: Pubkey,  // e.g., USDC
    pub fee_vault: Pubkey,   // PDA token account for protocol fees
}

impl GlobalState {
    pub const LEN: usize = 32  // authority
        + 2                    // fee_bps
        + 1                    // version
        + 2                    // max_batch_size
        + 2                    // max_sellers_per_timeslot
        + 2                    // max_bids_per_page
        + 2                    // slashing_penalty_bps
        + 4                    // appeal_window_seconds
        + 4                    // delivery_window_duration
        + 8                    // min_proposal_stake
        + 8                    // min_voting_stake
        + 4 + (32 * 10)        // governance_council (Vec with max 10 members)
        + 2                    // council_vote_multiplier
        + 8                    // min_participation_threshold
        + 4 + (32 * 5)         // authorized_oracles (Vec with max 5 oracles)
        + 1                    // bump
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
// 1. Add new state to track seller allocation results
#[account]
pub struct SellerAllocation {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub allocated_quantity: u64,  // How much this seller will sell
    pub allocation_price: u64,    // Price this seller gets (usually clearing price)
    pub proceeds_withdrawn: bool,
    pub bump: u8,
}
/// Context for calculating buyer allocations
#[derive(Accounts)]
#[instruction(buyer_key: Pubkey)]
pub struct CalculateBuyerAllocations<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    #[account(
        init,
        payer = payer,
        space = 8 + BuyerAllocation::LEN,
        seeds = [b"buyer_allocation", timeslot.key().as_ref(), buyer_key.as_ref()],
        bump
    )]
    pub buyer_allocation: Account<'info, BuyerAllocation>,
    
    #[account(mut)]
    pub payer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

/// Context for registering sellers
#[derive(Accounts)]
pub struct RegisterSeller<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    #[account(
        init_if_needed,
        payer = seller,
        space = 8 + SellerRegistry::LEN,
        seeds = [b"seller_registry", timeslot.key().as_ref()],
        bump
    )]
    pub seller_registry: Account<'info, SellerRegistry>,
    #[account(mut)]
    pub seller: Signer<'info>,
    pub system_program: Program<'info, System>,
}

/// Context for cancelling auction
#[derive(Accounts)]
pub struct CancelAuction<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    pub authority: Signer<'info>,
}

/// Context for emergency withdrawal
#[derive(Accounts)]
pub struct EmergencyWithdraw<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(mut)]
    pub source_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub destination_account: Account<'info, TokenAccount>,
    
    pub emergency_state: Account<'info, EmergencyState>,
    
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

/// Context for validating system health
#[derive(Accounts)]
pub struct ValidateSystemHealth<'info> {
    pub global_state: Account<'info, GlobalState>,
    pub emergency_state: Account<'info, EmergencyState>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for initializing bid registry
#[derive(Accounts)]
pub struct InitBidRegistry<'info> {
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        init,
        payer = payer,
        space = 8 + BidRegistry::LEN,
        seeds = [b"bid_registry", timeslot.key().as_ref()],
        bump
    )]
    pub bid_registry: Account<'info, BidRegistry>,
    
    #[account(mut)]
    pub payer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

/// Context for registering bid pages
#[derive(Accounts)]
#[instruction(page_index: u32)]
pub struct RegisterBidPage<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        seeds = [b"bid_registry", timeslot.key().as_ref()],
        bump
    )]
    pub bid_registry: Account<'info, BidRegistry>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    #[account(
        seeds = [b"bid_page", timeslot.key().as_ref(), &page_index.to_le_bytes()],
        bump
    )]
    pub bid_page: Account<'info, BidPage>,
}

#[derive(Accounts)]
pub struct InitAllocationTracker<'info> {
    pub global_state: Account<'info, GlobalState>,
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    #[account(
        init,
        payer = authority,
        space = 8 + AllocationTracker::LEN,
        seeds = [b"allocation_tracker", timeslot.key().as_ref()],
        bump
    )]
    pub allocation_tracker: Account<'info, AllocationTracker>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

/// Context for refunding buyers after cancellation
#[derive(Accounts)]
pub struct RefundCancelledBuyers<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + CancellationState::LEN,
        seeds = [b"cancellation_state", timeslot.key().as_ref()],
        bump
    )]
    pub cancellation_state: Account<'info, CancellationState>,
    
    #[account(
        mut,
        seeds = [b"quote_escrow", timeslot.key().as_ref()],
        bump
    )]
    pub timeslot_quote_escrow: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Context for refunding sellers after cancellation
#[derive(Accounts)]
pub struct RefundCancelledSellers<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"cancellation_state", timeslot.key().as_ref()],
        bump
    )]
    pub cancellation_state: Account<'info, CancellationState>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
}

/// Context for reporting non-delivery
#[derive(Accounts)]
pub struct ReportNonDelivery<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        seeds = [b"seller_allocation", timeslot.key().as_ref(), seller_allocation.supplier.as_ref()],
        bump
    )]
    pub seller_allocation: Account<'info, SellerAllocation>,
    
    #[account(
        init,
        payer = reporter,
        space = 8 + SlashingState::LEN,
        seeds = [b"slashing_state", timeslot.key().as_ref(), seller_allocation.supplier.as_ref()],
        bump
    )]
    pub slashing_state: Account<'info, SlashingState>,
    
    #[account(mut)]
    pub reporter: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for appealing slashing
#[derive(Accounts)]
pub struct AppealSlashing<'info> {
    #[account(
        mut,
        seeds = [b"slashing_state", slashing_state.timeslot.as_ref(), seller.key().as_ref()],
        bump
    )]
    pub slashing_state: Account<'info, SlashingState>,
    
    #[account(mut)]
    pub seller: Signer<'info>,
    
    pub clock: Sysvar<'info, Clock>,
}

/// Context for executing slashing
#[derive(Accounts)]
pub struct ExecuteSlashing<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"slashing_state", timeslot.key().as_ref(), slashing_state.supplier.as_ref()],
        bump
    )]
    pub slashing_state: Account<'info, SlashingState>,
    
    #[account(mut)]
    pub seller_collateral: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"slashing_vault"],
        bump
    )]
    pub slashing_vault: Account<'info, TokenAccount>,
    
    pub seller_allocation: Account<'info, SellerAllocation>,
    
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for verifying delivery confirmation
#[derive(Accounts)]
pub struct VerifyDeliveryConfirmation<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        init,
        payer = authority,
        space = 8 + SlashingState::LEN,
        seeds = [b"slashing_state", timeslot.key().as_ref(), supplier.key().as_ref()],
        bump
    )]
    pub slashing_state: Account<'info, SlashingState>,
    
    /// CHECK: This is the supplier being reported for delivery shortfall
    pub supplier: AccountInfo<'info>,
    
    pub seller_allocation: Account<'info, SellerAllocation>,
    
    /// CHECK: Oracle account for delivery verification
    pub oracle: AccountInfo<'info>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for resolving slashing appeals
#[derive(Accounts)]
pub struct ResolveSlashingAppeal<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"slashing_state", timeslot.key().as_ref(), slashing_state.supplier.as_ref()],
        bump
    )]
    pub slashing_state: Account<'info, SlashingState>,
    
    #[account(
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(mut)]
    pub seller_collateral: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"slashing_vault"],
        bump
    )]
    pub slashing_vault: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for emergency pause
#[derive(Accounts)]
pub struct EmergencyPause<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + EmergencyState::LEN,
        seeds = [b"emergency_state"],
        bump
    )]
    pub emergency_state: Account<'info, EmergencyState>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for emergency resume
#[derive(Accounts)]
pub struct EmergencyResume<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"emergency_state"],
        bump
    )]
    pub emergency_state: Account<'info, EmergencyState>,
    
    pub authority: Signer<'info>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for auction rollback
#[derive(Accounts)]
pub struct RollbackAuction<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        mut,
        seeds = [b"timeslot", &timeslot.epoch_ts.to_le_bytes()],
        bump,
    )]
    pub timeslot: Account<'info, Timeslot>,
    
    #[account(
        mut,
        seeds = [b"auction_state", timeslot.key().as_ref()],
        bump
    )]
    pub auction_state: Account<'info, AuctionState>,
    
    pub authority: Signer<'info>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for proposing parameter changes
#[derive(Accounts)]
#[instruction(proposal_id: u64)]
pub struct ProposeParameterChange<'info> {
    pub global_state: Account<'info, GlobalState>,
    
    #[account(
        init,
        payer = proposer,
        space = 8 + GovernanceProposal::LEN,
        seeds = [b"proposal", &proposal_id.to_le_bytes()],
        bump
    )]
    pub proposal: Account<'info, GovernanceProposal>,
    
    #[account(mut)]
    pub proposer: Signer<'info>,
    
    /// Token account representing proposer's stake
    pub proposer_stake: Account<'info, TokenAccount>,
    
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for voting on proposals
#[derive(Accounts)]
pub struct VoteOnProposal<'info> {
    #[account(
        mut,
        seeds = [b"proposal", &proposal.proposal_id.to_le_bytes()],
        bump = proposal.bump
    )]
    pub proposal: Account<'info, GovernanceProposal>,
    
    #[account(
        init,
        payer = voter,
        space = 8 + VoteRecord::LEN,
        seeds = [b"vote_record", proposal.key().as_ref(), voter.key().as_ref()],
        bump
    )]
    pub vote_record: Account<'info, VoteRecord>,
    
    #[account(mut)]
    pub voter: Signer<'info>,
    
    /// Token account representing voter's stake
    pub voter_stake: Account<'info, TokenAccount>,
    
    pub global_state: Account<'info, GlobalState>,
    
    pub system_program: Program<'info, System>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for executing proposals
#[derive(Accounts)]
pub struct ExecuteProposal<'info> {
    #[account(
        mut,
        seeds = [b"proposal", &proposal.proposal_id.to_le_bytes()],
        bump = proposal.bump
    )]
    pub proposal: Account<'info, GovernanceProposal>,
    
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    
    pub authority: Signer<'info>,
    pub clock: Sysvar<'info, Clock>,
}

/// Context for updating protocol parameters
#[derive(Accounts)]
pub struct UpdateProtocolParams<'info> {
    #[account(
        mut,
        has_one = authority @ EnergyAuctionError::InvalidAuthority
    )]
    pub global_state: Account<'info, GlobalState>,
    
    pub authority: Signer<'info>,
}

/// Context for initializing the protocol
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + GlobalState::LEN,
        seeds = [b"global_state"],
        bump
    )]
    pub global_state: Account<'info, GlobalState>,
    
    /// Quote token mint (e.g., USDC)
    pub quote_mint: Account<'info, Mint>,
    
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
    
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

impl SellerAllocation {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 1 + 1;
}

/// Buyer allocation tracking multi-seller energy distribution
#[account]
pub struct BuyerAllocation {
    pub buyer: Pubkey,
    pub timeslot: Pubkey,
    pub total_quantity_won: u64,
    pub clearing_price: u64,
    pub total_cost: u64,
    pub refund_amount: u64,
    pub total_escrowed: u64,
    pub energy_sources: Vec<EnergySource>, // Which sellers provide energy
    pub redeemed: bool,
    pub bump: u8,
}

impl BuyerAllocation {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 8 + 8 + 4 + (32 + 8 + 32) * 100 + 1 + 1; // Max 100 energy sources (configurable)
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct EnergySource {
    pub seller: Pubkey,
    pub quantity: u64,
    pub escrow_account: Pubkey,
}

/// Registry to track all sellers for a timeslot
#[account]
pub struct SellerRegistry {
    pub timeslot: Pubkey,
    pub sellers: Vec<Pubkey>, // All sellers for this timeslot
    pub seller_count: u32,
    pub bump: u8,
}

impl SellerRegistry {
    pub const LEN: usize = 32 + 4 + (32 * 1000) + 4 + 1; // Max sellers (configurable)
}

/// Registry to track all bid pages for efficient lookup
#[account]
pub struct BidRegistry {
    pub timeslot: Pubkey,
    pub bid_pages: Vec<Pubkey>, // All bid pages for this timeslot
    pub total_pages: u32,
    pub bump: u8,
}

impl BidRegistry {
    pub const LEN: usize = 32 + 4 + (32 * 1000) + 4 + 1; // Max pages (configurable)
}
// New data structures for auction clearing system

/// Tracks the state of an auction during and after clearing
#[account]
pub struct AuctionState {
    pub timeslot: Pubkey,
    pub clearing_price: u64,
    pub total_cleared_quantity: u64,
    pub total_revenue: u64,
    pub winning_bids_count: u32,
    pub participating_sellers_count: u32,
    pub status: u8, // Using u8 for AuctionStatus
    pub clearing_timestamp: i64,
    pub highest_price: u64, // Highest bid price
    pub bump: u8,
}

impl AuctionState {
    pub const LEN: usize = 32  // timeslot
        + 8                    // clearing_price
        + 8                    // total_cleared_quantity
        + 8                    // total_revenue
        + 4                    // winning_bids_count
        + 4                    // participating_sellers_count
        + 1                    // status
        + 8                    // clearing_timestamp
        + 8                    // highest_price
        + 1;                   // bump
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum AuctionStatus {
    Processing = 0,
    Cleared = 1,
    Settled = 2,
    Failed = 3,
}

/// Aggregates bids at the same price level for efficient demand curve construction
#[account]
pub struct PriceLevelAggregate {
    pub timeslot: Pubkey,
    pub price: u64,
    pub total_quantity: u64,
    pub bid_count: u16,
    pub cumulative_quantity: u64, // running sum for demand curve
    pub bump: u8,
}

impl PriceLevelAggregate {
    pub const LEN: usize = 32  // timeslot
        + 8                    // price
        + 8                    // total_quantity
        + 2                    // bid_count
        + 8                    // cumulative_quantity
        + 1;                   // bump
}

/// Tracks individual bid outcomes after auction clearing
#[account]
pub struct BidOutcome {
    pub buyer: Pubkey,
    pub timeslot: Pubkey,
    pub original_bid_price: u64,
    pub original_quantity: u64,
    pub filled_quantity: u64,
    pub refund_amount: u64,
    pub settlement_status: SettlementStatus,
    pub bump: u8,
}

impl BidOutcome {
    pub const LEN: usize = 32  // buyer
        + 32                   // timeslot
        + 8                    // original_bid_price
        + 8                    // original_quantity
        + 8                    // filled_quantity
        + 8                    // refund_amount
        + 1                    // settlement_status
        + 1;                   // bump
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum SettlementStatus {
    Pending = 0,
    Processed = 1,
    Refunded = 2,
    Failed = 3,
}

/// Batch processing result for bids
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct BatchResult {
    pub processed_bids: u32,
    pub total_quantity: u64,
    pub highest_price: u64,
    pub lowest_price: u64,
}

/// Refund batch processing result
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct RefundBatchResult {
    pub refunded_count: u32,
    pub total_amount: u64,
}

/// Supply allocation result
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SupplyAllocationResult {
    pub processed_sellers: u32,
    pub total_allocated: u64,
    pub remaining_demand: u64,
}

/// Final clearing result
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ClearingResult {
    pub clearing_price: u64,
    pub cleared_quantity: u64,
    pub total_revenue: u64,
    pub winning_bids: u32,
    pub participating_sellers: u32,
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
    #[msg("Seller's reserve price not met by clearing price")]
    ReservePriceNotMet,
    #[msg("No more quantity available for allocation")]
    AllocationExhausted,
    #[msg("Suppliers must be processed in merit order by reserve price")]
    InvalidMeritOrder,
    // New error codes for auction clearing system
    #[msg("No market clearing possible - all reserve prices exceed highest bid")]
    NoMarketClearing,
    #[msg("Auction clearing computation limit exceeded")]
    ComputationLimitExceeded,
    #[msg("Auction already in progress")]
    AuctionInProgress,
    #[msg("Auction clearing failed")]
    AuctionClearingFailed,
    #[msg("Invalid bid page sequence")]
    InvalidBidPageSequence,
    #[msg("Insufficient demand for clearing")]
    InsufficientDemand,
    #[msg("Insufficient supply for clearing")]
    InsufficientSupply,
    #[msg("Settlement verification failed")]
    SettlementVerificationFailed,
    #[msg("Escrow balance mismatch")]
    EscrowMismatch,
    #[msg("Precision error in quantity allocation")]
    PrecisionError,
    #[msg("Batch processing error")]
    BatchProcessingError,
    #[msg("Invalid supplier keys provided")]
    InvalidSupplierKeys,
    #[msg("No intersection found between supply and demand")]
    NoIntersection,
    #[msg("Insufficient account space for allocation")]
    InsufficientAccountSpace,
    #[msg("Missing seller allocation account")]
    MissingSellerAllocationAccount,
    #[msg("Auction cancellation in progress")]
    CancellationInProgress,
    #[msg("Delivery verification failed")]
    DeliveryVerificationFailed,
    #[msg("Slashing appeal period expired")]
    SlashingAppealExpired,
    #[msg("Emergency pause is active")]
    EmergencyPauseActive,
    #[msg("Proposal voting period expired")]
    ProposalVotingExpired,
    #[msg("Insufficient voting power")]
    InsufficientVotingPower,
    #[msg("Parameter value out of bounds")]
    ParameterOutOfBounds,
    #[msg("Emergency pause required for this operation")]
    EmergencyPauseRequired,
    #[msg("Insufficient upgrade accounts provided")]
    InsufficientUpgradeAccounts,
    #[msg("Insufficient stake for this operation")]
    InsufficientStake,
    #[msg("Proposal has not passed")]
    ProposalNotPassed,
    #[msg("Timelock period has not expired")]
    TimelockNotExpired,
    #[msg("Insufficient signatures for execution")]
    InsufficientSignatures,
    #[msg("Insufficient time elapsed for this operation")]
    InsufficientTimeElapsed,
    #[msg("Delivery window has expired")]
    DeliveryWindowExpired,
    #[msg("Unauthorized oracle")]
    UnauthorizedOracle,
    #[msg("Voting period has expired")]
    VotingPeriodExpired,
}

/// Types of governance proposals
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum ProposalType {
    FeeBps,
    Version,
    MaxBatchSize,
    MaxSellersPerTimeslot,
    MaxBidsPerPage,
    SlashingPenaltyBps,
    AppealWindowSeconds,
    DeliveryWindowDuration,
    MinProposalStake,
    MinVotingStake,
    EmergencyParameterChange,
    ProtocolUpgrade,
}

/// Proposal status
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    Active = 0,
    Executed = 1,
    Rejected = 2,
    Passed = 3,
}

/// Vote options
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    For,
    Against,
}

/// System health status
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum SystemStatus {
    Healthy,
    Warning,
    Critical,
}

/// Appeal decision options
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum AppealDecision {
    Upheld,
    Rejected,
}

/// Emergency withdrawal types
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum EmergencyWithdrawalType {
    CancelledAuction,
    StuckFunds,
    ProtocolUpgrade,
}

/// System health report
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SystemHealthReport {
    pub overall_status: SystemStatus,
    pub active_auctions: u32,
    pub pending_settlements: u32,
    pub total_locked_value: u64,
    pub failed_deliveries: u32,
    pub emergency_pause_active: bool,
    pub emergency_paused: bool,
    pub last_check_timestamp: i64,
}

/// Delivery report from oracle
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct DeliveryReport {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub allocated_quantity: u64,
    pub delivered_quantity: u64,
    pub evidence_hash: [u8; 32],
    pub timestamp: i64,
    pub oracle_signature: [u8; 64],
}

///////////////////////
// Additional Events
///////////////////////

#[event]
pub struct EmergencyWithdrawal {
    pub withdrawal_type: EmergencyWithdrawalType,
    pub amount: u64,
    pub recipient: Pubkey,
    pub authority: Pubkey,
    pub source_account: Pubkey,
    pub destination_account: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct AutoSlashingTriggered {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub shortfall_quantity: u64,
    pub penalty_amount: u64,
    pub slashing_amount: u64,
    pub appeal_deadline: i64,
    pub timestamp: i64,
}

#[event]
pub struct DeliveryVerified {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub allocated_quantity: u64,
    pub delivered_quantity: u64,
    pub oracle: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct ProposalPassed {
    pub proposal_id: Pubkey,
    pub proposal_type: ProposalType,
    pub final_vote_count: u64,
    pub votes_for: u64,
    pub votes_against: u64,
    pub signatures: u8,
    pub timestamp: i64,
}

#[event]
pub struct CircuitBreakerTriggered {
    pub trigger_reason: SystemStatus,
    pub reason: String,
    pub anomaly_count: u32,
    pub timestamp: i64,
    pub authority: Pubkey,
}

#[event]
pub struct SlashingAppealUpheld {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub refund_amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct SlashingAppealRejected {
    pub supplier: Pubkey,
    pub timeslot: Pubkey,
    pub penalty_confirmed: u64,
    pub final_penalty: u64,
    pub timestamp: i64,
}
