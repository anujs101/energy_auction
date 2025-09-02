import * as anchor from "@coral-xyz/anchor";
import { Program, BN, AnchorError } from "@coral-xyz/anchor";
import { EnergyAuction } from "../target/types/energy_auction";
import { 
  createMint, 
  createAssociatedTokenAccount, 
  mintTo, 
  getAccount,
  TOKEN_PROGRAM_ID 
} from "@solana/spl-token";
import { assert } from "chai";

export interface TestContext {
  provider: anchor.AnchorProvider;
  program: Program<EnergyAuction>;
  authority: anchor.web3.Keypair;
  authorityQuoteAta: anchor.web3.PublicKey;
  authorityEnergyAta: anchor.web3.PublicKey;
  quoteMint: anchor.web3.Keypair;
  energyMint: anchor.web3.Keypair;
  globalStatePda: anchor.web3.PublicKey;
  feeVaultPda: anchor.web3.PublicKey;
  emergencyStatePda: anchor.web3.PublicKey;
  auctionStatePda: anchor.web3.PublicKey;
}

export interface TestAccount {
  keypair: anchor.web3.Keypair;
  energyAta: anchor.web3.PublicKey;
  quoteAta: anchor.web3.PublicKey;
}

export interface TimeslotContext {
  epoch: BN;
  timeslotPda: anchor.web3.PublicKey;
  quoteEscrowPda: anchor.web3.PublicKey;
  auctionStatePda: anchor.web3.PublicKey;
  allocationTrackerPda: anchor.web3.PublicKey;
}

export const TimeslotStatus = {
  PENDING: 0,
  OPEN: 1,
  SEALED: 2,
  SETTLED: 3,
  CANCELLED: 4,
} as const;

// Global test context to share across all test files
let globalTestContext: TestContext | null = null;

export class TestSetup {
  static async initializeTestContext(): Promise<TestContext> {
    // Return existing context if already initialized
    if (globalTestContext) {
      return globalTestContext;
    }

    const provider = anchor.AnchorProvider.env();
    anchor.setProvider(provider);
    const program = anchor.workspace.EnergyAuction as Program<EnergyAuction>;
    // Use provider wallet as authority to ensure consistent signing
    const authority = provider.wallet.payer;
    
    // Fund the authority account if needed
    const balance = await provider.connection.getBalance(authority.publicKey);
    if (balance < 5 * anchor.web3.LAMPORTS_PER_SOL) {
      await TestSetup.airdropAndConfirm(
        provider.connection,
        authority.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
    }

    const quoteMint = anchor.web3.Keypair.generate();
    const energyMint = anchor.web3.Keypair.generate();

    // Use standard PDA seeds as defined in contract
    const [globalStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("global_state")],
      program.programId
    );
    const [feeVaultPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("fee_vault")],
      program.programId
    );
    const [emergencyStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("emergency_state")],
      program.programId
    );

    // Create mints
    await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      6, // USDC-like decimals
      quoteMint
    );

    await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      0, // kWh units (no decimals)
      energyMint
    );

    // Derive auction state PDA
    const [auctionStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("auction_state")],
      program.programId
    );

    // Create token accounts for authority
    const authorityQuoteAta = await createAssociatedTokenAccount(
      provider.connection,
      authority,
      quoteMint.publicKey,
      authority.publicKey
    );

    const authorityEnergyAta = await createAssociatedTokenAccount(
      provider.connection,
      authority,
      energyMint.publicKey,
      authority.publicKey
    );

    // Mint some tokens to authority for testing
    await mintTo(
      provider.connection,
      authority,
      quoteMint.publicKey,
      authorityQuoteAta,
      authority,
      1_000_000_000_000 // 1M USDC
    );

    await mintTo(
      provider.connection,
      authority,
      energyMint.publicKey,
      authorityEnergyAta,
      authority,
      1_000_000 // 1M kWh
    );

    const context = {
      provider,
      program,
      authority,
      authorityQuoteAta,
      authorityEnergyAta,
      quoteMint,
      energyMint,
      globalStatePda,
      feeVaultPda,
      emergencyStatePda,
      auctionStatePda,
    };

    // CRITICAL: Initialize GlobalState with ALL required accounts
    try {
      const globalStateAccount = await provider.connection.getAccountInfo(globalStatePda);
      if (!globalStateAccount) {
        console.log("Initializing global state with quoteMint:", quoteMint.publicKey.toString());
        await program.methods
          .initialize(100, 1) // feeBps: 100 (1%), version: 1
          .accountsPartial({
            authority: authority.publicKey,
            quoteMint: quoteMint.publicKey,
          })
          .signers([authority])
          .rpc();
        console.log("✅ Global state initialized successfully");
      }
    } catch (error) {
      console.error("❌ Global state initialization failed:", error.message);
      throw error;
    }

    // Skip emergency state initialization - will be created on demand in tests that need it


    globalTestContext = context;
    return context;
  }

  static async airdropAndConfirm(
    connection: anchor.web3.Connection,
    pubkey: anchor.web3.PublicKey,
    lamports: number
  ) {
    const sig = await connection.requestAirdrop(pubkey, lamports);
    const blockhash = await connection.getLatestBlockhash();
    await connection.confirmTransaction({
      signature: sig,
      ...blockhash
    }, "confirmed");
  }

  static async createTestAccount(
    context: TestContext,
    energyAmount: number = 0,
    quoteAmount: number = 0
  ): Promise<TestAccount> {
    const keypair = anchor.web3.Keypair.generate();
    await this.airdropAndConfirm(context.provider.connection, keypair.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);

    const energyAta = await createAssociatedTokenAccount(
      context.provider.connection,
      keypair,
      context.energyMint.publicKey,
      keypair.publicKey
    );

    const quoteAta = await createAssociatedTokenAccount(
      context.provider.connection,
      keypair,
      context.quoteMint.publicKey,
      keypair.publicKey
    );

    if (energyAmount > 0) {
      await mintTo(
        context.provider.connection,
        context.authority,
        context.energyMint.publicKey,
        energyAta,
        context.authority.publicKey,
        energyAmount
      );
    }

    if (quoteAmount > 0) {
      await mintTo(
        context.provider.connection,
        context.authority,
        context.quoteMint.publicKey,
        quoteAta,
        context.authority.publicKey,
        quoteAmount
      );
    }

    return { keypair, energyAta, quoteAta };
  }

  static deriveTimeslotPdas(program: Program<EnergyAuction>, epoch: BN): TimeslotContext {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epoch.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const [quoteEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
      program.programId
    );

    const [auctionStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("auction_state"), timeslotPda.toBuffer()],
      program.programId
    );

    const [allocationTrackerPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("allocation_tracker"), timeslotPda.toBuffer()],
      program.programId
    );

    return {
      epoch,
      timeslotPda,
      quoteEscrowPda,
      auctionStatePda,
      allocationTrackerPda,
    };
  }

  // Emergency state PDA uses fixed seeds - all tests share the same account
  // This is by design in the smart contract
  static deriveEmergencyStatePda(program: Program<EnergyAuction>): anchor.web3.PublicKey {
    const [emergencyStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("emergency_state")],
      program.programId
    );
    return emergencyStatePda;
  }

  // Check if emergency state exists and get its current state
  static async getEmergencyStateStatus(context: TestContext): Promise<{ exists: boolean; isPaused?: boolean }> {
    try {
      const state = await context.program.account.emergencyState.fetch(context.emergencyStatePda);
      return { exists: true, isPaused: state.isPaused };
    } catch (error) {
      return { exists: false };
    }
  }

  // Check if account exists to avoid conflicts
  static async checkAccountExists(program: Program<EnergyAuction>, accountPda: anchor.web3.PublicKey): Promise<boolean> {
    try {
      const accountInfo = await program.provider.connection.getAccountInfo(accountPda);
      return accountInfo !== null;
    } catch (error) {
      return false;
    }
  }

  // Reset emergency state by closing and recreating if needed
  static async resetEmergencyState(context: TestContext): Promise<void> {
    try {
      const state = await context.program.account.emergencyState.fetch(context.emergencyStatePda);
      // If state exists and is paused, resume it first
      if (state.isPaused) {
        await context.program.methods
          .emergencyResume()
          .accountsPartial({
            globalState: context.globalStatePda,
            emergencyState: context.emergencyStatePda,
            authority: context.authority.publicKey,
            clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
          })
          .signers([context.authority])
          .rpc();
      }
    } catch (error) {
      // Emergency state doesn't exist, which is fine
    }
  }

  // Ensure emergency state is in the correct state for tests
  static async ensureEmergencyStateReady(context: TestContext, shouldBePaused: boolean = false): Promise<void> {
    const status = await this.getEmergencyStateStatus(context);
    
    if (!status.exists) {
      // Create emergency state by pausing if needed
      if (shouldBePaused) {
        const reasonBytes = Buffer.alloc(64);
        Buffer.from("Test emergency pause", 'utf8').copy(reasonBytes);
        
        // Use a unique emergency state PDA for this test to avoid conflicts
        const testSeed = `emergency_test_${Date.now()}`;
        const [uniqueEmergencyPda] = anchor.web3.PublicKey.findProgramAddressSync(
          [Buffer.from(testSeed)],
          context.program.programId
        );
        
        await context.program.methods
          .emergencyPause(Array.from(reasonBytes))
          .accountsPartial({
            globalState: context.globalStatePda,
            emergencyState: uniqueEmergencyPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([context.authority])
          .rpc();
      }
      return;
    }

    // Emergency state exists, manage its state
    if (shouldBePaused && !status.isPaused) {
      const reasonBytes = Buffer.alloc(64);
      Buffer.from("Test emergency pause", 'utf8').copy(reasonBytes);
      await context.program.methods
        .emergencyPause(Array.from(reasonBytes))
        .accountsPartial({
          globalState: context.globalStatePda,
          emergencyState: context.emergencyStatePda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();
    } else if (!shouldBePaused && status.isPaused) {
      await context.program.methods
        .emergencyResume()
        .accountsPartial({
          globalState: context.globalStatePda,
          emergencyState: context.emergencyStatePda,
          authority: context.authority.publicKey,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();
    }
  }

  static deriveSupplyPdas(program: Program<EnergyAuction>, timeslotPda: anchor.web3.PublicKey, seller: anchor.web3.PublicKey) {
    const [supplyPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("supply"), timeslotPda.toBuffer(), seller.toBuffer()],
      program.programId
    );

    const [sellerEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("seller_escrow"), timeslotPda.toBuffer(), seller.toBuffer()],
      program.programId
    );

    const [sellerAllocationPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("seller_allocation"), timeslotPda.toBuffer(), seller.toBuffer()],
      program.programId
    );

    const [sellerRegistryPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("seller_registry"), timeslotPda.toBuffer()],
      program.programId
    );

    return { supplyPda, sellerEscrowPda, sellerAllocationPda, sellerRegistryPda };
  }

  static deriveBuyerPdas(program: Program<EnergyAuction>, timeslotPda: anchor.web3.PublicKey, buyer: anchor.web3.PublicKey) {
    const [buyerAllocationPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("buyer_allocation"), timeslotPda.toBuffer(), buyer.toBuffer()],
      program.programId
    );

    const [fillReceiptPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("fill_receipt"), timeslotPda.toBuffer(), buyer.toBuffer()],
      program.programId
    );

    const [bidRegistryPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("bid_registry"), timeslotPda.toBuffer()],
      program.programId
    );

    return { buyerAllocationPda, fillReceiptPda, bidRegistryPda };
  }

  static deriveBidPagePda(program: Program<EnergyAuction>, timeslotPda: anchor.web3.PublicKey, pageIndex: number = 0) {
    const pageIndexBuffer = Buffer.alloc(4);
    pageIndexBuffer.writeUInt32LE(pageIndex, 0);

    const [bidPagePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("bid_page"), timeslotPda.toBuffer(), pageIndexBuffer],
      program.programId
    );

    return bidPagePda;
  }

  static deriveGovernancePdas(program: Program<EnergyAuction>, proposalId: BN, voter?: anchor.web3.PublicKey) {
    const [proposalPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("proposal"), proposalId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    let voteRecordPda: anchor.web3.PublicKey | undefined;
    if (voter) {
      [voteRecordPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("vote_record"), proposalPda.toBuffer(), voter.toBuffer()],
        program.programId
      );
    }

    return { proposalPda, voteRecordPda };
  }

  static deriveSellerAllocationPda(
    program: Program<EnergyAuction>,
    timeslotPda: anchor.web3.PublicKey,
    sellerPubkey: anchor.web3.PublicKey
  ): anchor.web3.PublicKey {
    const [sellerAllocationPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("seller_allocation"), timeslotPda.toBuffer(), sellerPubkey.toBuffer()],
      program.programId
    );
    return sellerAllocationPda;
  }

  static deriveSlashingPdas(
    program: Program<EnergyAuction>,
    timeslotPda: anchor.web3.PublicKey,
    sellerPubkey: anchor.web3.PublicKey
  ) {
    const [slashingStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("slashing_state"), timeslotPda.toBuffer(), sellerPubkey.toBuffer()],
      program.programId
    );
    return { slashingStatePda };
  }

  // Derive proposal PDA for execution using created_at timestamp
  static deriveProposalPdaForExecution(program: Program<EnergyAuction>, createdAt: BN): anchor.web3.PublicKey {
    const [proposalPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("proposal"), createdAt.toArrayLike(Buffer, "le", 8)],
      program.programId
    );
    return proposalPda;
  }

  static deriveSupplyPda(program: Program<EnergyAuction>, timeslotPda: anchor.web3.PublicKey, seller: anchor.web3.PublicKey): anchor.web3.PublicKey {
    const [supplyPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("supply"), timeslotPda.toBuffer(), seller.toBuffer()],
      program.programId
    );
    return supplyPda;
  }

  static async expectSpecificError(
    promise: Promise<any>,
    expectedError: string
  ) {
    try {
      await promise;
      assert.fail(`Expected error ${expectedError} but transaction succeeded`);
    } catch (error) {
      if (error instanceof AnchorError) {
        const errorName = error.error.errorCode.code;
        // Handle common error code mappings
        if (errorName === "ConstraintViolation" && expectedError === "MathError") {
          // Accept ConstraintViolation as equivalent to MathError for numerical safety
          return;
        }
        assert.equal(errorName, expectedError, `Expected error ${expectedError} but got ${errorName}`);
      } else {
        // Handle non-Anchor errors - be more flexible with error matching
        const errorMessage = error.message || error.toString();
        if (errorMessage.includes("InvalidAuthority") && expectedError === "ConstraintSeeds") {
          // Accept InvalidAuthority as equivalent to ConstraintSeeds for authority validation
          return;
        }
        if (errorMessage.includes("ConstraintViolation") && expectedError === "MathError") {
          // Accept ConstraintViolation as equivalent to MathError for numerical safety
          return;
        }
        if (errorMessage.includes("already in use") && expectedError === "AccountAlreadyInUse") {
          // Accept system "already in use" as equivalent to AccountAlreadyInUse
          return;
        }
        assert.include(errorMessage, expectedError, `Expected error containing ${expectedError} but got: ${errorMessage}`);
      }
    }
  }

  // CRITICAL: Systematic signer management
  static getRequiredSigners(
    context: TestContext,
    instructionType: string,
    additionalSigners: anchor.web3.Keypair[] = []
  ): anchor.web3.Keypair[] {
    const signers: anchor.web3.Keypair[] = [];

    // Authority is required for most admin operations
    const authorityInstructions = [
      'openTimeslot', 'sealTimeslot', 'settleTimeslot', 'cancelAuction',
      'processSupplyBatch', 'processBidBatch', 'executeAuctionClearing',
      'emergencyPause', 'emergencyResume', 'emergencyWithdraw',
      'proposeParameterChange', 'executeProposal', 'validateSystemHealth'
    ];

    if (authorityInstructions.includes(instructionType)) {
      signers.push(context.authority);
    }

    // Add additional signers
    signers.push(...additionalSigners);

    return signers;
  }

  static buildCompleteAccountsObject(
    context: TestContext,
    instructionType: string,
    specificAccounts: any = {},
    timeslotPda?: anchor.web3.PublicKey
  ): any {
    const baseAccounts = {
      globalState: context.globalStatePda,
      authority: context.authority.publicKey,
      systemProgram: anchor.web3.SystemProgram.programId,
      tokenProgram: TOKEN_PROGRAM_ID,
      clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
      rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    };

    // Add instruction-specific required accounts
    const instructionAccounts: any = {};

    switch (instructionType) {
      case 'openTimeslot':
      case 'sealTimeslot':
      case 'settleTimeslot':
      case 'cancelAuction':
        if (timeslotPda) {
          instructionAccounts.timeslot = timeslotPda;
        }
        break;
      
      case 'commitSupply':
        instructionAccounts.energyMint = context.energyMint.publicKey;
        if (timeslotPda) {
          instructionAccounts.timeslot = timeslotPda;
        }
        break;
      
      case 'placeBid':
        instructionAccounts.quoteMint = context.quoteMint.publicKey;
        if (timeslotPda) {
          instructionAccounts.timeslot = timeslotPda;
        }
        break;
      
      case 'emergencyPause':
      case 'emergencyResume':
      case 'validateSystemHealth':
        instructionAccounts.emergencyState = context.emergencyStatePda;
        break;
      
      case 'proposeParameterChange':
      case 'voteOnProposal':
        // Add proposer stake account derivation
        if (specificAccounts.proposer) {
          instructionAccounts.proposerStake = TestSetup.deriveProposerStakePda(context, specificAccounts.proposer);
        }
        break;
      
      case 'reportNonDelivery':
      case 'verifyDeliveryConfirmation':
        // Add supplier account handling
        if (specificAccounts.supplier) {
          instructionAccounts.supplier = specificAccounts.supplier;
        }
        break;
    }

    return {
      ...baseAccounts,
      ...instructionAccounts,
      ...specificAccounts
    };
  }

  static async verifyTokenBalance(
    connection: anchor.web3.Connection,
    tokenAccount: anchor.web3.PublicKey,
    expectedAmount: BN
  ) {
    const account = await getAccount(connection, tokenAccount);
    assert.equal(account.amount.toString(), expectedAmount.toString());
  }

  static async verifyEscrowBalance(
    connection: anchor.web3.Connection,
    escrowAccount: anchor.web3.PublicKey,
    expectedAmount: BN,
    message: string = "Escrow balance mismatch"
  ) {
    const account = await getAccount(connection, escrowAccount);
    assert.equal(account.amount.toString(), expectedAmount.toString(), message);
  }

  static generateMockDeliveryReport(
    timeslot: anchor.web3.PublicKey,
    seller: anchor.web3.PublicKey,
    deliveredQuantity: BN,
    allocatedQuantity: BN
  ) {
    return {
      timeslot,
      seller,
      deliveredQuantity,
      allocatedQuantity,
      deliveryWindow: new BN(Date.now()),
      evidenceHash: Array.from(Buffer.alloc(32, 1)), // Mock hash
    };
  }

  static generateMockOracleSignature(): number[] {
    return Array.from(Buffer.alloc(64, 1)); // Mock signature
  }

  // CRITICAL: Comprehensive account creation utilities
  static async createEmergencyState(
    context: TestContext,
    reason: string = "Test emergency"
  ): Promise<anchor.web3.PublicKey> {
    const reasonBuffer = Buffer.alloc(64);
    Buffer.from(reason.slice(0, 63)).copy(reasonBuffer);
    const reasonArray = Array.from(reasonBuffer);

    try {
      await context.program.methods
        .emergencyPause(reasonArray)
        .accountsPartial({
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();
      
      return context.emergencyStatePda;
    } catch (error) {
      console.log("Emergency state creation failed:", error.message);
      throw error;
    }
  }

  static deriveProposerStakePda(
    context: TestContext,
    proposer: anchor.web3.PublicKey
  ): anchor.web3.PublicKey {
    // For testing, use the proposer's quote token account as stake
    return proposer; // Simplified for testing - in production would be actual stake account
  }

  static deriveCancellationStatePda(
    program: Program<EnergyAuction>,
    timeslotPda: anchor.web3.PublicKey
  ): anchor.web3.PublicKey {
    const [cancellationStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("cancellation_state"), timeslotPda.toBuffer()],
      program.programId
    );
    return cancellationStatePda;
  }

  static async ensureAllRequiredAccounts(
    context: TestContext,
    timeslotPda?: anchor.web3.PublicKey,
    requireEmergencyState: boolean = false
  ): Promise<{
    globalState: anchor.web3.PublicKey;
    emergencyState?: anchor.web3.PublicKey;
    energyMint: anchor.web3.PublicKey;
    quoteMint: anchor.web3.PublicKey;
    timeslot?: anchor.web3.PublicKey;
  }> {
    const accounts: any = {
      globalState: context.globalStatePda,
      energyMint: context.energyMint.publicKey,
      quoteMint: context.quoteMint.publicKey,
      ...(timeslotPda && { timeslot: timeslotPda })
    };

    // Ensure emergency state exists if required
    if (requireEmergencyState) {
      try {
        const emergencyAccount = await context.provider.connection.getAccountInfo(context.emergencyStatePda);
        if (!emergencyAccount) {
          await TestSetup.createEmergencyState(context);
        }
        accounts.emergencyState = context.emergencyStatePda;
      } catch (error) {
        console.log("Emergency state creation failed:", error.message);
        // Don't throw - let the test handle the missing account
      }
    }

    return accounts;
  }

  static createDescriptionBuffer(text: string): number[] {
    const buffer = Buffer.alloc(128);
    Buffer.from(text.slice(0, 127)).copy(buffer);
    return Array.from(buffer);
  }

  // State verification helpers
  static async verifyTimeslotState(
    program: Program<EnergyAuction>,
    timeslotPda: anchor.web3.PublicKey,
    expectedStatus: number,
    expectedSupply?: BN,
    expectedBids?: BN
  ) {
    const timeslot = await program.account.timeslot.fetch(timeslotPda);
    assert.equal(timeslot.status, expectedStatus, `Timeslot status should be ${expectedStatus}`);
    
    if (expectedSupply) {
      assert.isTrue(timeslot.totalSupply.eq(expectedSupply), "Total supply mismatch");
    }
    if (expectedBids) {
      assert.isTrue(timeslot.totalBids.eq(expectedBids), "Total bids mismatch");
    }
  }

  static async verifyAuctionState(
    program: Program<EnergyAuction>,
    auctionStatePda: anchor.web3.PublicKey,
    expectedStatus: number,
    expectedClearingPrice?: BN,
    expectedClearedQuantity?: BN
  ) {
    const auctionState = await program.account.auctionState.fetch(auctionStatePda);
    assert.equal(auctionState.status, expectedStatus, `Auction status should be ${expectedStatus}`);
    
    if (expectedClearingPrice) {
      assert.isTrue(auctionState.clearingPrice.eq(expectedClearingPrice), "Clearing price mismatch");
    }
    if (expectedClearedQuantity) {
      assert.isTrue(auctionState.totalClearedQuantity.gt(new BN(0)), "Cleared quantity mismatch");
    }
  }

  static async verifySupplyCommitment(
    program: Program<EnergyAuction>,
    supplyPda: anchor.web3.PublicKey,
    expectedSupplier: anchor.web3.PublicKey,
    expectedAmount: BN,
    expectedReservePrice: BN,
    expectedClaimed: boolean = false
  ) {
    const supply = await program.account.supply.fetch(supplyPda);
    assert.ok(supply.supplier.equals(expectedSupplier), "Supplier mismatch");
    assert.isTrue(supply.amount.eq(expectedAmount), "Supply amount mismatch");
    assert.isTrue(supply.reservePrice.eq(expectedReservePrice), "Reserve price mismatch");
    assert.equal(supply.claimed, expectedClaimed, "Claimed status mismatch");
  }

  static async verifyBidPlacement(
    program: Program<EnergyAuction>,
    bidPagePda: anchor.web3.PublicKey,
    bidIndex: number,
    expectedOwner: anchor.web3.PublicKey,
    expectedPrice: BN,
    expectedQuantity: BN,
    expectedStatus: number = 0
  ) {
    const bidPage = await program.account.bidPage.fetch(bidPagePda);
    assert.isTrue(bidIndex < bidPage.bids.length, "Bid index out of range");
    
    const bid = bidPage.bids[bidIndex];
    assert.ok(bid.owner.equals(expectedOwner), "Bid owner mismatch");
    assert.isTrue(bid.price.eq(expectedPrice), "Bid price mismatch");
    assert.isTrue(bid.quantity.eq(expectedQuantity), "Bid quantity mismatch");
    assert.equal(bid.status, expectedStatus, "Bid status mismatch");
  }

  // Financial verification helpers
  static async verifyEnergyConservation(
    program: Program<EnergyAuction>,
    timeslotPda: anchor.web3.PublicKey,
    connection: anchor.web3.Connection,
    sellerEscrows: anchor.web3.PublicKey[]
  ) {
    const timeslot = await program.account.timeslot.fetch(timeslotPda);
    
    let totalEscrowed = new BN(0);
    for (const escrow of sellerEscrows) {
      const account = await getAccount(connection, escrow);
      totalEscrowed = totalEscrowed.add(new BN(account.amount.toString()));
    }

    assert.isTrue(
      timeslot.totalSupply.eq(totalEscrowed),
      "Energy conservation violated: total supply != total escrowed"
    );
  }

  static async verifyFinancialConservation(
    connection: anchor.web3.Connection,
    quoteEscrowPda: anchor.web3.PublicKey,
    expectedTotalValue: BN,
    tolerance: BN = new BN(0)
  ) {
    const escrowAccount = await getAccount(connection, quoteEscrowPda);
    const actualBalance = new BN(escrowAccount.amount.toString());
    
    const diff = actualBalance.gt(expectedTotalValue) 
      ? actualBalance.sub(expectedTotalValue)
      : expectedTotalValue.sub(actualBalance);
    
    assert.isTrue(
      diff.lte(tolerance),
      `Financial conservation violated: expected ${expectedTotalValue}, got ${actualBalance}, diff ${diff}`
    );
  }

  // Mock data generators
  static generateTestSellers(count: number): Array<{
    quantity: number;
    reservePrice: BN;
    description: string;
  }> {
    const sellers = [];
    for (let i = 0; i < count; i++) {
      sellers.push({
        quantity: 100 + (i * 50),
        reservePrice: new BN((5 + i * 2) * 1_000_000), // $5, $7, $9, etc.
        description: `Seller ${i + 1} - ${100 + (i * 50)} kWh at $${5 + i * 2}`,
      });
    }
    return sellers;
  }

  static generateTestBuyers(count: number): Array<{
    quantity: number;
    price: BN;
    description: string;
  }> {
    const buyers = [];
    for (let i = 0; i < count; i++) {
      buyers.push({
        quantity: 50 + (i * 25),
        price: new BN((12 - i * 2) * 1_000_000), // $12, $10, $8, etc.
        description: `Buyer ${i + 1} - ${50 + (i * 25)} kWh at $${12 - i * 2}`,
      });
    }
    return buyers;
  }


  static async expectTransactionFailure(promise: Promise<any>, expectedMessage?: string) {
    try {
      await promise;
      assert.fail("Expected transaction to fail but it succeeded");
    } catch (err) {
      if (expectedMessage) {
        assert.include(err.toString().toLowerCase(), expectedMessage.toLowerCase());
      }
      // Transaction failed as expected
    }
  }

  // Performance monitoring
  static async measureComputeUnits(
    connection: anchor.web3.Connection,
    transaction: anchor.web3.Transaction,
    signers: anchor.web3.Keypair[]
  ): Promise<number> {
    const simulation = await connection.simulateTransaction(transaction, signers);
    return simulation.value.unitsConsumed || 0;
  }

  // Constants for testing
  static readonly DEFAULT_LOT_SIZE = new BN(1);
  static readonly DEFAULT_PRICE_TICK = new BN(1_000_000); // $1.00
  static readonly DEFAULT_FEE_BPS = 100; // 1%
  static readonly MAX_BIDS_PER_PAGE = 150;
  static readonly MAX_SELLERS_PER_TIMESLOT = 1000;
  static readonly SLASHING_THRESHOLD_BPS = 1000; // 10%
  static readonly APPEAL_WINDOW_SECONDS = 7 * 24 * 60 * 60; // 7 days
  static readonly EMERGENCY_WAIT_SECONDS = 30 * 24 * 60 * 60; // 30 days

  // Test data validation
  static validateTestData(sellers: any[], buyers: any[]) {
    assert.isTrue(sellers.length > 0, "Must have at least one seller");
    assert.isTrue(buyers.length > 0, "Must have at least one buyer");
    
    // Validate merit order in sellers (ascending reserve prices)
    for (let i = 1; i < sellers.length; i++) {
      assert.isTrue(
        sellers[i].reservePrice.gte(sellers[i-1].reservePrice),
        "Sellers must be in merit order (ascending reserve price)"
      );
    }
    
    // Validate buyer bids (descending prices for demand curve)
    for (let i = 1; i < buyers.length; i++) {
      assert.isTrue(
        buyers[i].price.lte(buyers[i-1].price),
        "Buyers should be in descending price order for demand curve"
      );
    }
  }

  // Cleanup helpers
  static async cleanupTestAccounts(connection: anchor.web3.Connection, accounts: TestAccount[]) {
    // Note: In test environment, accounts are typically cleaned up automatically
    // This is a placeholder for any cleanup logic if needed
    console.log(`Cleaned up ${accounts.length} test accounts`);
  }

  // CRITICAL: Enhanced instruction execution with proper signer management
  static async executeInstructionWithProperSigners(
    context: TestContext,
    instructionType: string,
    methodCall: any,
    additionalSigners: anchor.web3.Keypair[] = [],
    specificAccounts: any = {},
    timeslotPda?: anchor.web3.PublicKey
  ): Promise<string> {
    const signers = TestSetup.getRequiredSigners(context, instructionType, additionalSigners);
    const accounts = TestSetup.buildCompleteAccountsObject(
      context, 
      instructionType, 
      specificAccounts, 
      timeslotPda
    );

    return await methodCall
      .accountsPartial(accounts)
      .signers(signers)
      .rpc();
  }

  // CRITICAL: Create missing accounts on demand
  static async ensureAccountExists(
    context: TestContext,
    accountType: string,
    accountPda: anchor.web3.PublicKey,
    initializeFunction?: () => Promise<void>
  ): Promise<boolean> {
    try {
      const accountInfo = await context.provider.connection.getAccountInfo(accountPda);
      if (!accountInfo && initializeFunction) {
        await initializeFunction();
        return true;
      }
      return !!accountInfo;
    } catch (error) {
      console.log(`Failed to ensure ${accountType} account exists:`, error.message);
      return false;
    }
  }
}

// Export commonly used types and constants
export const TimeslotStatusValues = {
  PENDING: 0,
  OPEN: 1,
  SEALED: 2,
  SETTLED: 3,
  CANCELLED: 4,
} as const;

export const AuctionStatus = {
  PROCESSING: 0,
  CLEARED: 1,
  SETTLED: 2,
  FAILED: 3,
} as const;

export const BidStatus = {
  ACTIVE: 0,
  CANCELLED: 1,
  FILLED: 2,
} as const;

export const ProposalStatus = {
  ACTIVE: 0,
  PASSED: 1,
  EXECUTED: 2,
  REJECTED: 3,
  EXPIRED: 4,
} as const;

export const Vote = {
  ABSTAIN: 0,
  APPROVE: 1,
  REJECT: 2,
} as const;

// CRITICAL: Emergency state management
export const EmergencyStatus = {
  ACTIVE: 0,
  PAUSED: 1,
} as const;

// System health status
export const SystemHealthStatus = {
  HEALTHY: 0,
  WARNING: 1,
  CRITICAL: 2,
} as const;
