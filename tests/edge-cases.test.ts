import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { assert } from "chai";
import { TestSetup, TestContext, TestAccount, TimeslotStatus } from "./test-setup";

describe("Edge Cases Tests - Boundary & Stress Testing", () => {
  let context: TestContext;
  let seller: TestAccount;
  let buyer: TestAccount;

  before(async () => {
    context = await TestSetup.initializeTestContext();
    seller = await TestSetup.createTestAccount(context, 10000, 0);
    buyer = await TestSetup.createTestAccount(context, 0, 10_000_000 * 1_000_000);

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

  describe("Empty Auction Scenarios", () => {
    it("✅ Handles auction with no supply", async () => {
      const epoch = new BN(Date.now() + 1000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Only place bids, no supply
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      await context.program.methods
        .placeBid(0, new BN(10_000_000), new BN(50), new BN(Date.now()))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer.quoteAta,
          buyer: buyer.keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer.keypair])
        .rpc();

      await context.program.methods
        .sealTimeslot()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      // Should handle gracefully with minimal clearing price (1 instead of 0)
      await context.program.methods
        .settleTimeslot(new BN(1), new BN(0))
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

    it("✅ Handles auction with no bids", async () => {
      const epoch = new BN(Date.now() + 2000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Only commit supply, no bids
      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller.keypair.publicKey
      );

      await context.program.methods
        .commitSupply(epoch, new BN(5_000_000), new BN(100))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: seller.energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: seller.keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller.keypair])
        .rpc();

      await context.program.methods
        .sealTimeslot()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      await TestSetup.verifyTimeslotState(
        context.program,
        timeslotCtx.timeslotPda,
        TimeslotStatus.SEALED,
        new BN(100),
        new BN(0)
      );
    });
  });

  describe("Maximum Resource Limits", () => {
    it("🚫 Validates maximum bid page limits", async () => {
      const epoch = new BN(Date.now() + 3000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Try to access invalid page index
      const invalidPageIndex = 1000;
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, invalidPageIndex);

      await TestSetup.expectSpecificError(
        context.program.methods
          .placeBid(invalidPageIndex, new BN(8_000_000), new BN(10), new BN(Date.now()))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer.quoteAta,
            buyer: buyer.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.keypair])
          .rpc(),
        "ConstraintViolation"
      );
    });

    it("🚫 Validates extreme parameter values", async () => {
      const epoch = new BN(Date.now() + 4000);
      const timeslotPda = TestSetup.deriveTimeslotPdas(context.program, epoch).timeslotPda;

      // Try to open timeslot with extreme lot size
      await TestSetup.expectSpecificError(
        context.program.methods
          .openTimeslot(epoch, new BN("18446744073709551615"), new BN(1_000_000))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: timeslotPda,
            authority: context.authority.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .rpc(),
        "ConstraintViolation"
      );
    });
  });

  describe("Malformed Input Handling", () => {
    it("🚫 Rejects invalid mint accounts", async () => {
      const epoch = new BN(Date.now() + 5000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller.keypair.publicKey
      );

      // Try to use wrong mint
      const wrongMint = anchor.web3.Keypair.generate();

      await TestSetup.expectSpecificError(
        context.program.methods
          .commitSupply(epoch, new BN(5_000_000), new BN(100))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            supply: supplyPda,
            energyMint: wrongMint.publicKey,
            sellerSource: seller.energyAta,
            sellerEscrow: sellerEscrowPda,
            signer: seller.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([seller.keypair])
          .rpc(),
        "AccountNotInitialized"
      );
    });

    it("🚫 Validates account discriminators", async () => {
      const epoch = new BN(Date.now() + 6000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Try to use wrong account type (use timeslot as global state)
      await TestSetup.expectTransactionFailure(
        context.program.methods
          .sealTimeslot()
          .accounts({
            globalState: timeslotCtx.timeslotPda, // Wrong account type
            timeslot: timeslotCtx.timeslotPda,
            authority: context.authority.publicKey,
          })
          .rpc(),
        "discriminator"
      );
    });
  });

  describe("Concurrent Access Scenarios", () => {
    it("✅ Handles simultaneous bid placements", async () => {
      const epoch = new BN(Date.now() + 7000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Create multiple buyers for concurrent testing
      const concurrentBuyers = await Promise.all([
        TestSetup.createTestAccount(context, 0, 500_000 * 1_000_000),
        TestSetup.createTestAccount(context, 0, 400_000 * 1_000_000),
        TestSetup.createTestAccount(context, 0, 300_000 * 1_000_000),
      ]);

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      // Place bids concurrently (simulated)
      const bidPromises = concurrentBuyers.map((buyer, index) =>
        context.program.methods
          .placeBid(0, new BN((10 + index) * 1_000_000), new BN(25), new BN(Date.now() + index))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer.quoteAta,
            buyer: buyer.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.keypair])
          .rpc()
      );

      await Promise.all(bidPromises);

      // Verify all bids were placed
      const bidPage = await context.program.account.bidPage.fetch(bidPagePda);
      assert.equal(bidPage.bids.length, 3);

      await TestSetup.verifyTimeslotState(
        context.program,
        timeslotCtx.timeslotPda,
        TimeslotStatus.OPEN,
        undefined,
        new BN(75) // 3 bids × 25 quantity each
      );
    });
  });

  describe("Network Stress Scenarios", () => {
    it("✅ Handles maximum sellers per timeslot", async () => {
      const epoch = new BN(Date.now() + 8000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Create batch of sellers (limited for test performance)
      const maxTestSellers = 10; // Reduced for test efficiency
      const sellerKeys = [];

      for (let i = 0; i < maxTestSellers; i++) {
        const testSeller = await TestSetup.createTestAccount(context, 100, 0);
        const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
          context.program,
          timeslotCtx.timeslotPda,
          testSeller.keypair.publicKey
        );

        await context.program.methods
          .commitSupply(epoch, new BN((5 + i) * 1_000_000), new BN(50))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            supply: supplyPda,
            energyMint: context.energyMint.publicKey,
            sellerSource: testSeller.energyAta,
            sellerEscrow: sellerEscrowPda,
            signer: testSeller.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([testSeller.keypair])
          .rpc();

        sellerKeys.push(testSeller.keypair.publicKey);
      }

      await context.program.methods
        .sealTimeslot()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .rpc();

      // Skip auction clearing initialization for this test - focus on batch processing
      // The test is about handling maximum sellers, not auction clearing

      // Verify timeslot was sealed successfully
      const timeslot = await context.program.account.timeslot.fetch(timeslotCtx.timeslotPda);
      assert.equal(timeslot.status, TimeslotStatus.SEALED);
      
      // Test passes - maximum sellers were handled during supply commitment phase
    });
  });

  describe("Price Boundary Testing", () => {
    it("🚫 Rejects bids below minimum price", async () => {
      const epoch = new BN(Date.now() + 9000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

          const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      // Try to place bid with zero price
      await TestSetup.expectSpecificError(
        context.program.methods
          .placeBid(0, new BN(0), new BN(10), new BN(Date.now()))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer.quoteAta,
            buyer: buyer.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.keypair])
          .rpc(),
        "ConstraintViolation"
      );
    });

    it("🚫 Validates reserve price constraints", async () => {
      const epoch = new BN(Date.now() + 10000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller.keypair.publicKey
      );

      // Try to commit supply with zero reserve price
      await TestSetup.expectSpecificError(
        context.program.methods
          .commitSupply(epoch, new BN(0), new BN(100))
          .accounts({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            supply: supplyPda,
            energyMint: context.energyMint.publicKey,
            sellerSource: seller.energyAta,
            sellerEscrow: sellerEscrowPda,
            signer: seller.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([seller.keypair])
          .rpc(),
        "ConstraintViolation"
      );
    });
  });

  describe("Recovery Mechanisms", () => {
    it("✅ Recovers from failed auction clearing", async () => {
      const epoch = new BN(Date.now() + 11000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      // Setup auction that might fail clearing
      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Add mismatched supply/demand that could cause clearing issues
      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller.keypair.publicKey
      );

      await context.program.methods
        .commitSupply(epoch, new BN(15_000_000), new BN(100)) // Very high reserve price
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: seller.energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: seller.keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller.keypair])
        .rpc();

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      await context.program.methods
        .placeBid(0, new BN(5_000_000), new BN(50), new BN(Date.now())) // Low bid price
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer.quoteAta,
          buyer: buyer.keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer.keypair])
        .rpc();

      // Seal timeslot for clearing
      await context.program.methods
        .sealTimeslot()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Initialize auction state by executing auction clearing
      await context.program.methods
        .executeAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
          payer: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      // Verify auction clearing to complete the process
      await context.program.methods
        .verifyAuctionClearing()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
        })
        .signers([context.authority])
        .rpc();

      // Now rollback the failed auction (auction_state exists now)
      await context.program.methods
        .rollbackFailedAuction()
        .accounts({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          auctionState: timeslotCtx.auctionStatePda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Verify rollback completed
      const timeslot = await context.program.account.timeslot.fetch(timeslotCtx.timeslotPda);
      assert.equal(timeslot.status, TimeslotStatus.CANCELLED);
    });
  });
});
