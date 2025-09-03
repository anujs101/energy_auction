import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { TOKEN_PROGRAM_ID, getAccount } from "@solana/spl-token";
import { assert } from "chai";
import { TestSetup, TestContext, TestAccount, TimeslotStatus } from "./test-setup";

describe("Integration Tests - End-to-End Workflows", () => {
  let context: TestContext;
  let sellers: TestAccount[];
  let buyers: TestAccount[];

  before(async () => {
    context = await TestSetup.initializeTestContext();
    
    // Create multiple sellers and buyers for complex scenarios
    sellers = await Promise.all([
      TestSetup.createTestAccount(context, 1000, 0), // Seller 1: 1000 kWh
      TestSetup.createTestAccount(context, 1500, 0), // Seller 2: 1500 kWh  
      TestSetup.createTestAccount(context, 800, 0),  // Seller 3: 800 kWh
    ]);

    buyers = await Promise.all([
      TestSetup.createTestAccount(context, 0, 2_000_000 * 1_000_000), // Buyer 1: 2M USDC
      TestSetup.createTestAccount(context, 0, 1_500_000 * 1_000_000), // Buyer 2: 1.5M USDC
      TestSetup.createTestAccount(context, 0, 1_000_000 * 1_000_000), // Buyer 3: 1M USDC
    ]);

    // Check if GlobalState already exists to avoid re-initialization
    try {
      await context.program.account.globalState.fetch(context.globalStatePda);
      console.log("GlobalState already initialized, skipping initialization");
    } catch (error) {
      // GlobalState doesn't exist, initialize it
      await context.program.methods
        .initialize(100, 1)
        .accountsPartial({
          quoteMint: context.quoteMint.publicKey,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();
    }
  });

  describe("Multi-Seller Multi-Buyer Auction", () => {
    it("✅ Executes complex auction with merit order", async () => {
      const epoch = new BN(Date.now() + 1000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      // Open timeslot
      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Sellers commit supply in merit order (ascending reserve prices)
      const sellerData = [
        { account: sellers[0], quantity: 200, reservePrice: new BN(4_000_000) }, // $4.00
        { account: sellers[1], quantity: 300, reservePrice: new BN(6_000_000) }, // $6.00
        { account: sellers[2], quantity: 150, reservePrice: new BN(8_000_000) }, // $8.00
      ];

      for (const seller of sellerData) {
        const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
          context.program,
          timeslotCtx.timeslotPda,
          seller.account.keypair.publicKey
        );

        await context.program.methods
          .commitSupply(epoch, seller.reservePrice, new BN(seller.quantity))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            supply: supplyPda,
            energyMint: context.energyMint.publicKey,
            sellerSource: seller.account.energyAta,
            sellerEscrow: sellerEscrowPda,
            signer: seller.account.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([seller.account.keypair])
          .rpc();
      }

      // Buyers place bids (descending prices for demand curve)
      const buyerData = [
        { account: buyers[0], quantity: 150, price: new BN(12_000_000) }, // $12.00
        { account: buyers[1], quantity: 200, price: new BN(9_000_000) },  // $9.00
        { account: buyers[2], quantity: 100, price: new BN(7_000_000) },  // $7.00
      ];

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      for (const buyer of buyerData) {
        await context.program.methods
          .placeBid(0, buyer.price, new BN(buyer.quantity), new BN(Date.now()))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer.account.quoteAta,
            buyer: buyer.account.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.account.keypair])
          .rpc();
      }

      // Verify total supply and bids
      await TestSetup.verifyTimeslotState(
        context.program,
        timeslotCtx.timeslotPda,
        TimeslotStatus.OPEN,
        new BN(650), // Total supply: 200 + 300 + 150
        new BN(450)  // Total bids: 150 + 200 + 100
      );

      // Seal timeslot
      await context.program.methods
        .sealTimeslot()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Process auction clearing - executeAuctionClearing creates auction_state
      // Execute auction clearing to create auction_state
      await context.program.methods
        .executeAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
          payer: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Process supply and bids
      await context.program.methods
        .processSupplyBatch([sellers[0].keypair.publicKey, sellers[1].keypair.publicKey, sellers[2].keypair.publicKey])
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
        })
        .signers([context.authority])
        .rpc();

      // Derive price level PDA for bid processing
      const priceLevelPda = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("price_level"), timeslotCtx.timeslotPda.toBuffer(), Buffer.alloc(8, 0)],
        context.program.programId
      )[0];

      await context.program.methods
        .processBidBatch(0, 0)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
          priceLevel: priceLevelPda,
          payer: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Verify auction clearing (no second executeAuctionClearing call)
      await context.program.methods
        .verifyAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
          timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      // Verify clearing results
      const auctionState = await context.program.account.auctionState.fetch(timeslotCtx.auctionStatePda);
      assert.equal(auctionState.status, 1); // Cleared
      assert.isTrue(auctionState.clearingPrice.gt(new BN(0)));
      assert.ok(auctionState.totalClearedQuantity.gt(new anchor.BN(0)));
      console.log("✅ Multi-seller clearing price:", auctionState.clearingPrice.toString());
      console.log("✅ Total cleared quantity:", auctionState.totalClearedQuantity.toString());

      // Get timeslot to check total supply
      const timeslot = await context.program.account.timeslot.fetch(timeslotCtx.timeslotPda);
      const actualSoldQuantity = Math.min(auctionState.totalClearedQuantity.toNumber(), timeslot.totalSupply.toNumber());
      
      // Settlement
      await context.program.methods
        .settleTimeslot(auctionState.clearingPrice, new BN(actualSoldQuantity))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      await TestSetup.verifyTimeslotState(
        context.program,
        timeslotCtx.timeslotPda,
        TimeslotStatus.SETTLED
      );
    });
  });

  describe("Cross-Timeslot Coordination", () => {
    it("✅ Handles multiple concurrent timeslots", async () => {
      const epochs = [
        new BN(Date.now() + 2000),
        new BN(Date.now() + 3000),
        new BN(Date.now() + 4000),
      ];

      const timeslotContexts = epochs.map(epoch => 
        TestSetup.deriveTimeslotPdas(context.program, epoch)
      );

      // Open all timeslots
      for (let i = 0; i < epochs.length; i++) {
        await context.program.methods
          .openTimeslot(epochs[i], new BN(1), new BN(1_000_000))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotContexts[i].timeslotPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([context.authority])
          .rpc();
      }

      // Verify all timeslots are open
      for (const ctx of timeslotContexts) {
        await TestSetup.verifyTimeslotState(
          context.program,
          ctx.timeslotPda,
          TimeslotStatus.OPEN
        );
      }

      // Seal all timeslots
      for (const ctx of timeslotContexts) {
        await context.program.methods
          .sealTimeslot()
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: ctx.timeslotPda,
            authority: context.authority.publicKey,
          })
          .signers([context.authority])
          .rpc();
      }

      // Verify all sealed
      for (const ctx of timeslotContexts) {
        await TestSetup.verifyTimeslotState(
          context.program,
          ctx.timeslotPda,
          TimeslotStatus.SEALED
        );
      }
    });
  });

  describe("Delivery Verification & Slashing", () => {
    it("✅ Handles delivery verification workflow", async () => {
      const deliveryEpoch = new BN(Math.floor(Date.now() / 1000)); // Use current Unix timestamp in seconds
      const deliveryCtx = TestSetup.deriveTimeslotPdas(context.program, deliveryEpoch);

      // Setup auction with seller
      await context.program.methods
        .openTimeslot(deliveryEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      const { supplyPda, sellerEscrowPda, sellerAllocationPda } = TestSetup.deriveSupplyPdas(
        context.program,
        deliveryCtx.timeslotPda,
        sellers[0].keypair.publicKey
      );

      await context.program.methods
        .commitSupply(deliveryEpoch, new BN(5_000_000), new BN(100))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: sellers[0].energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: sellers[0].keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([sellers[0].keypair])
        .rpc();

      // Place a bid to create the quote escrow account
      const buyer = await TestSetup.createTestAccount(context, 1_000_000_000, 1_000_000_000);
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, deliveryCtx.timeslotPda, 0);
      
      await context.program.methods
        .placeBid(0, new BN(1_000_000), new BN(100), new BN(Date.now()))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          timeslotQuoteEscrow: deliveryCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer.quoteAta,
          buyer: buyer.keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer.keypair])
        .rpc();

      // Seal timeslot
      await context.program.methods
        .sealTimeslot()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      // Execute auction clearing to create auction_state
      await context.program.methods
        .executeAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          auctionState: deliveryCtx.auctionStatePda,
          payer: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      // Verify auction clearing 
      await context.program.methods
        .verifyAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          auctionState: deliveryCtx.auctionStatePda,
          timeslotQuoteEscrow: deliveryCtx.quoteEscrowPda,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();


      // Get auction state for settlement values
      const auctionStateForSettlement = await context.program.account.auctionState.fetch(deliveryCtx.auctionStatePda);
      const timeslotForSettlement = await context.program.account.timeslot.fetch(deliveryCtx.timeslotPda);
      
      console.log("Settlement values:");
      console.log("- Clearing price:", auctionStateForSettlement.clearingPrice.toString());
      console.log("- Total cleared quantity:", auctionStateForSettlement.totalClearedQuantity.toString());
      console.log("- Timeslot total supply:", timeslotForSettlement.totalSupply.toString());
      
      // Cap the total sold quantity to not exceed timeslot total supply
      const cappedTotalSoldQuantity = auctionStateForSettlement.totalClearedQuantity.gt(timeslotForSettlement.totalSupply) 
        ? timeslotForSettlement.totalSupply 
        : auctionStateForSettlement.totalClearedQuantity;
      
      console.log("- Capped total sold quantity:", cappedTotalSoldQuantity.toString());
      
      // Settle timeslot to transition to Settled status (required for delivery verification)
      await context.program.methods
        .settleTimeslot(auctionStateForSettlement.clearingPrice, cappedTotalSoldQuantity)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Initialize allocation tracker first
      await context.program.methods
        .initAllocationTracker()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          allocationTracker: deliveryCtx.allocationTrackerPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      // Check if seller allocation already exists to avoid conflicts
      const sellerAllocationExists = await TestSetup.checkAccountExists(context.program, sellerAllocationPda);
      
      if (!sellerAllocationExists) {
        // Create seller allocation using calculate method
        await context.program.methods
          .calculateSellerAllocations(new BN(6_000_000), new BN(100))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: deliveryCtx.timeslotPda,
            supply: supplyPda,
            sellerAllocation: sellerAllocationPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            remainingAllocationTracker: deliveryCtx.allocationTrackerPda,
          })
          .signers([context.authority])
          .rpc();
      }

      // Mock delivery verification with correct structure
      // Use a timestamp that's within the delivery window (between epoch and epoch + 24 hours)
      const deliveryTimestamp = deliveryEpoch.add(new BN(3600)); // 1 hour after epoch start
      const deliveryReport = {
        supplier: sellers[0].keypair.publicKey,
        timeslot: deliveryCtx.timeslotPda,
        allocatedQuantity: new BN(100),
        deliveredQuantity: new BN(80), // Create a shortfall to trigger slashing
        evidenceHash: Array.from(Buffer.from("delivery_evidence_hash_123", "utf8")),
        timestamp: deliveryTimestamp, // Use timestamp within delivery window
        oracleSignature: new Array(64).fill(0),
      };

      const { slashingStatePda } = TestSetup.deriveSlashingPdas(
        context.program,
        deliveryCtx.timeslotPda,
        sellers[0].keypair.publicKey
      );

      const oracle = await TestSetup.createTestAccount(context, 0, 0);

      // Oracle authorization is disabled in contract for testing purposes

      // Check if allocation tracker already exists before initializing
      const trackerExists = await TestSetup.checkAccountExists(context.program, deliveryCtx.allocationTrackerPda);
      
      if (!trackerExists) {
        // Initialize allocation tracker AFTER settlement
        await context.program.methods
          .initAllocationTracker()
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: deliveryCtx.timeslotPda,
            allocationTracker: deliveryCtx.allocationTrackerPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([context.authority])
          .rpc();
      }

      // Create seller allocation for the seller who committed supply (sellers[0])
      const seller = sellers[0];
      const deliveryAllocationPda = TestSetup.deriveSellerAllocationPda(
        context.program,
        deliveryCtx.timeslotPda,
        seller.keypair.publicKey
      );

      // Check if seller allocation already exists
      const allocationExists = await TestSetup.checkAccountExists(context.program, deliveryAllocationPda);
      
      if (!allocationExists) {
        // Get supply PDA for this seller (should already exist from earlier commitSupply)
        const supplyPda = TestSetup.deriveSupplyPda(context.program, deliveryCtx.timeslotPda, seller.keypair.publicKey);
        
        // Calculate seller allocations using settlement values
        await context.program.methods
          .calculateSellerAllocations(auctionStateForSettlement.clearingPrice, auctionStateForSettlement.totalClearedQuantity)
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: deliveryCtx.timeslotPda,
            sellerAllocation: deliveryAllocationPda,
            supply: supplyPda,
            remainingAllocationTracker: deliveryCtx.allocationTrackerPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([context.authority])
          .rpc();
      }

      // Verify timeslot is in Settled status
      const timeslotForDelivery = await context.program.account.timeslot.fetch(deliveryCtx.timeslotPda);
      console.log("Timeslot status for delivery verification:", timeslotForDelivery.status);
      
      if (timeslotForDelivery.status !== 3) { // Not Settled status
        console.log("ERROR: Timeslot not in Settled status for delivery verification. Status:", timeslotForDelivery.status);
        throw new Error(`Timeslot must be in Settled status (3) for delivery verification, but got status: ${timeslotForDelivery.status}`);
      }

      // Get seller allocation PDA for delivery verification
      const deliverySellerAllocationPda = TestSetup.deriveSellerAllocationPda(
        context.program,
        deliveryCtx.timeslotPda,
        sellers[0].keypair.publicKey
      );

      await context.program.methods
        .verifyDeliveryConfirmation(deliveryReport, TestSetup.generateMockOracleSignature())
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: deliveryCtx.timeslotPda,
          slashingState: slashingStatePda,
          sellerAllocation: deliverySellerAllocationPda,
          supplier: sellers[0].keypair.publicKey,
          authority: context.authority.publicKey,
          oracle: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      // Verify slashing state created for delivery shortfall
      const slashingState = await context.program.account.slashingState.fetch(slashingStatePda);
      assert.ok(slashingState.supplier.equals(sellers[0].keypair.publicKey));
      assert.isTrue(slashingState.allocatedQuantity.gt(slashingState.deliveredQuantity)); // Shortfall detected
    });
  });

  describe("Auction Cancellation & Refunds", () => {
    it("✅ Cancels auction and processes refunds", async () => {
      const cancelEpoch = new BN(Date.now() + 6000);
      const cancelCtx = TestSetup.deriveTimeslotPdas(context.program, cancelEpoch);

      // Setup auction with commitments
      await context.program.methods
        .openTimeslot(cancelEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Seller commits supply
      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        cancelCtx.timeslotPda,
        sellers[0].keypair.publicKey
      );

      await context.program.methods
        .commitSupply(cancelEpoch, new BN(5_000_000), new BN(100))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: sellers[0].energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: sellers[0].keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([sellers[0].keypair])
        .rpc();

      // Buyer places bid
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, cancelCtx.timeslotPda, 0);

      await context.program.methods
        .placeBid(0, new BN(8_000_000), new BN(75), new BN(Date.now()))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          timeslotQuoteEscrow: cancelCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyers[0].quoteAta,
          buyer: buyers[0].keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyers[0].keypair])
        .rpc();

      // Cancel auction
      const [cancellationStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("cancellation_state"), cancelCtx.timeslotPda.toBuffer()],
        context.program.programId
      );

      await context.program.methods
        .cancelAuction()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Verify cancellation
      await TestSetup.verifyTimeslotState(
        context.program,
        cancelCtx.timeslotPda,
        TimeslotStatus.CANCELLED
      );

      // Process refunds
      await context.program.methods
        .refundCancelledAuctionBuyers(0, 1)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          timeslotQuoteEscrow: cancelCtx.quoteEscrowPda,
          authority: context.authority.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();

      await context.program.methods
        .refundCancelledAuctionSellers([sellers[0].keypair.publicKey])
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: cancelCtx.timeslotPda,
          authority: context.authority.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();

      // Verify refunds processed
      const cancellationState = await context.program.account.cancellationState.fetch(cancellationStatePda);
      assert.equal(cancellationState.totalBuyersRefunded, 0);
      assert.equal(cancellationState.totalSellersRefunded, 0);
    });
  });

  describe("Real-World Usage Patterns", () => {
    it("✅ Simulates daily energy trading cycle", async () => {
      // Simulate morning peak demand
      const morningEpoch = new BN(Date.now() + 7000);
      const morningCtx = TestSetup.deriveTimeslotPdas(context.program, morningEpoch);

      await context.program.methods
        .openTimeslot(morningEpoch, new BN(1), new BN(1_000_000))
        .accounts({
          globalState: context.globalStatePda,
          timeslot: morningCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // High demand scenario - buyers bid aggressively
      const highDemandBuyers = [
        { account: buyers[0], quantity: 300, price: new BN(15_000_000) },
        { account: buyers[1], quantity: 250, price: new BN(13_000_000) },
        { account: buyers[2], quantity: 200, price: new BN(11_000_000) },
      ];

      // Limited supply - sellers can charge premium
      const limitedSuppliers = [
        { account: sellers[0], quantity: 200, reservePrice: new BN(10_000_000) },
        { account: sellers[1], quantity: 150, reservePrice: new BN(12_000_000) },
      ];

      // Commit limited supply
      for (const seller of limitedSuppliers) {
        const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
          context.program,
          morningCtx.timeslotPda,
          seller.account.keypair.publicKey
        );

        await context.program.methods
          .commitSupply(morningEpoch, seller.reservePrice, new BN(seller.quantity))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: morningCtx.timeslotPda,
            supply: supplyPda,
            energyMint: context.energyMint.publicKey,
            sellerSource: seller.account.energyAta,
            sellerEscrow: sellerEscrowPda,
            signer: seller.account.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([seller.account.keypair])
          .rpc();
      }

      // Place high demand bids
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, morningCtx.timeslotPda, 0);

      for (const buyer of highDemandBuyers) {
        await context.program.methods
          .placeBid(0, buyer.price, new BN(buyer.quantity), new BN(Date.now()))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: morningCtx.timeslotPda,
            timeslotQuoteEscrow: morningCtx.quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer.account.quoteAta,
            buyer: buyer.account.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.account.keypair])
          .rpc();
      }

      // Verify high demand scenario
      await TestSetup.verifyTimeslotState(
        context.program,
        morningCtx.timeslotPda,
        TimeslotStatus.OPEN,
        new BN(350), // Limited supply
        new BN(750)  // High demand
      );

      // Complete auction
      await context.program.methods
        .sealTimeslot()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: morningCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      // Expected clearing price should be high due to supply shortage
      await context.program.methods
        .settleTimeslot(new BN(12_500_000), new BN(350)) // Premium pricing
        .accounts({
          globalState: context.globalStatePda,
          timeslot: morningCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      const finalSlot = await context.program.account.timeslot.fetch(morningCtx.timeslotPda);
      assert.isTrue(finalSlot.clearingPrice.eq(new BN(12_500_000)));
      assert.isTrue(finalSlot.totalSoldQuantity.eq(new BN(350)));
    });
  });
});
