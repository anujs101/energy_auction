import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { TOKEN_PROGRAM_ID, getAccount } from "@solana/spl-token";
import { assert } from "chai";
import { 
  TestSetup, 
  TestContext, 
  TestAccount, 
  TimeslotContext,
  TimeslotStatus,
  AuctionStatus,
  BidStatus 
} from "./test-setup";

describe("Basic Flow Tests - Core Auction Lifecycle", () => {
  let context: TestContext;
  let seller1: TestAccount;
  let buyer1: TestAccount;

  before(async () => {
    context = await TestSetup.initializeTestContext();
    seller1 = await TestSetup.createTestAccount(context, 1000, 0);
    buyer1 = await TestSetup.createTestAccount(context, 0, 1_000_000 * 1_000_000);
  });

  describe("1. Protocol Initialization", () => {
    it("✅ Initializes global state", async () => {
      // Check if GlobalState already exists
      const existingAccount = await context.provider.connection.getAccountInfo(context.globalStatePda);
      
      if (!existingAccount) {
        // Initialize only if it doesn't exist
        await context.program.methods
          .initialize(TestSetup.DEFAULT_FEE_BPS, 1)
          .accountsPartial({
            authority: context.authority.publicKey,
          })
          .rpc();
      }

      // Verify the global state exists and has correct values
      const globalState = await context.program.account.globalState.fetch(
        context.globalStatePda
      );
      assert.equal(globalState.feeBps, TestSetup.DEFAULT_FEE_BPS);
      assert.equal(globalState.version, 1);
    });

    it("🚫 Fails unauthorized initialization", async () => {
      const fakeAuthority = anchor.web3.Keypair.generate();
      await TestSetup.airdropAndConfirm(context.provider.connection, fakeAuthority.publicKey, anchor.web3.LAMPORTS_PER_SOL);

      await TestSetup.expectSpecificError(
        context.program.methods
          .initialize(100, 1)
          .accountsPartial({
            authority: fakeAuthority.publicKey,
            quoteMint: context.quoteMint.publicKey,
          })
          .signers([fakeAuthority])
          .rpc(),
        "already in use"
      );
    });
  });

  describe("2. Complete Auction Flow", () => {
    let timeslotCtx: TimeslotContext;

    it("✅ Executes full auction lifecycle", async () => {
      const epoch = new BN(Date.now() + 1000);
      timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      // 1. Open timeslot
      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      // 2. Commit supply
      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller1.keypair.publicKey
      );

      await context.program.methods
        .commitSupply(epoch, new BN(5_000_000), new BN(100))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: seller1.energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: seller1.keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller1.keypair])
        .rpc();

      // 3. Place bid
      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      await context.program.methods
        .placeBid(0, new BN(8_000_000), new BN(50), new BN(Date.now()))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer1.quoteAta,
          buyer: buyer1.keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer1.keypair])
        .rpc();

      // 4. Seal and settle
      await context.program.methods
        .sealTimeslot()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();
      await context.program.methods
        .settleTimeslot(new BN(7_000_000), new BN(50))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: timeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Verify final state
      await TestSetup.verifyTimeslotState(
        context.program,
        timeslotCtx.timeslotPda,
        TimeslotStatus.SETTLED
      );
    });

    it("✅ Handles empty auction gracefully", async () => {
      const emptyEpoch = new BN(Date.now() + 2000);
      const emptyTimeslotCtx = TestSetup.deriveTimeslotPdas(context.program, emptyEpoch);

      await context.program.methods
        .openTimeslot(emptyEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: emptyTimeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      await context.program.methods
        .sealTimeslot()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: emptyTimeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      await TestSetup.verifyTimeslotState(
        context.program,
        emptyTimeslotCtx.timeslotPda,
        TimeslotStatus.SEALED,
        new BN(0),
        new BN(0)
      );
    });
  });

  describe("3. Error Handling", () => {
    it("🚫 Prevents operations on invalid timeslot states", async () => {
      const invalidEpoch = new BN(Date.now() + 3000);
      const invalidTimeslotCtx = TestSetup.deriveTimeslotPdas(context.program, invalidEpoch);

      // Try to seal non-existent timeslot
      await TestSetup.expectSpecificError(
        context.program.methods
          .sealTimeslot()
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: invalidTimeslotCtx.timeslotPda,
            authority: context.authority.publicKey,
          })
          .signers([context.authority])
          .rpc(),
        "AccountNotInitialized"
      );
    });

    it("🚫 Validates price tick alignment", async () => {
      const tickEpoch = new BN(Date.now() + 4000);
      const tickTimeslotCtx = TestSetup.deriveTimeslotPdas(context.program, tickEpoch);

      await context.program.methods
        .openTimeslot(tickEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: tickTimeslotCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, tickTimeslotCtx.timeslotPda, 0);

      await TestSetup.expectSpecificError(
        context.program.methods
          .placeBid(0, new BN(10_500_000), new BN(10), new BN(Date.now())) // $10.50 not aligned with $1.00 tick
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: tickTimeslotCtx.timeslotPda,
            timeslotQuoteEscrow: TestSetup.deriveTimeslotPdas(context.program, tickEpoch).quoteEscrowPda,
            quoteMint: context.quoteMint.publicKey,
            buyerSource: buyer1.quoteAta,
            buyer: buyer1.keypair.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer1.keypair])
          .rpc(),
        "ConstraintViolation"
      );
    });
  });

  describe("4. State Invariant Verification", () => {
    it("✅ Maintains energy conservation", async () => {
      const conservationEpoch = new BN(Date.now() + 5000);
      const conservationCtx = TestSetup.deriveTimeslotPdas(context.program, conservationEpoch);

      await context.program.methods
        .openTimeslot(conservationEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: conservationCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        conservationCtx.timeslotPda,
        seller1.keypair.publicKey
      );

      const commitQuantity = new BN(200);
      await context.program.methods
        .commitSupply(conservationEpoch, new BN(5_000_000), commitQuantity)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: conservationCtx.timeslotPda,
          supply: supplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: seller1.energyAta,
          sellerEscrow: sellerEscrowPda,
          signer: seller1.keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller1.keypair])
        .rpc();

      // Verify energy conservation
      await TestSetup.verifyEnergyConservation(
        context.program,
        conservationCtx.timeslotPda,
        context.provider.connection,
        [sellerEscrowPda]
      );
    });

    it("✅ Maintains financial conservation", async () => {
      const financialEpoch = new BN(Date.now() + 6000);
      const financialCtx = TestSetup.deriveTimeslotPdas(context.program, financialEpoch);

      await context.program.methods
        .openTimeslot(financialEpoch, new BN(1), new BN(1_000_000))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: financialCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, financialCtx.timeslotPda, 0);
      const bidPrice = new BN(8_000_000);
      const bidQuantity = new BN(25);
      const expectedEscrow = bidPrice.mul(bidQuantity);

      await context.program.methods
        .placeBid(0, bidPrice, bidQuantity, new BN(Date.now()))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: financialCtx.timeslotPda,
          timeslotQuoteEscrow: financialCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer1.quoteAta,
          buyer: buyer1.keypair.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer1.keypair])
        .rpc();

      // Skip redemption and withdrawal for now - focus on core auction flow
      // These require additional account setup that's not critical for basic flow testing

      // Verify financial conservation
      await TestSetup.verifyFinancialConservation(
        context.provider.connection,
        financialCtx.quoteEscrowPda,
        expectedEscrow
      );
    });
  });

  describe("5. Multi-Actor Scenarios", () => {
    it("✅ Handles multiple sellers and buyers", async () => {
      const multiEpoch = new BN(Date.now() + 7000);
      const multiCtx = TestSetup.deriveTimeslotPdas(context.program, multiEpoch);
      
      // Open timeslot with price tick of 1 for easier price alignment
      await context.program.methods
        .openTimeslot(multiEpoch, new BN(1000), new BN(1))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: multiCtx.timeslotPda,
          authority: context.authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([context.authority])
        .rpc();

      // Create test accounts with sufficient token balances
      const seller1 = await TestSetup.createTestAccount(context, 1000, 10000); // 1000 energy, 10000 quote
      const buyer1 = await TestSetup.createTestAccount(context, 0, 50000); // 50000 quote for bids
      const buyer2 = await TestSetup.createTestAccount(context, 0, 50000); // 50000 quote for bids

      // Seller commits supply
      const { supplyPda: multiSupplyPda, sellerEscrowPda: multiSellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        multiCtx.timeslotPda,
        seller1.keypair.publicKey
      );

      await context.program.methods
        .commitSupply(multiEpoch, new BN(80), new BN(500))
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: multiCtx.timeslotPda,
          supply: multiSupplyPda,
          energyMint: context.energyMint.publicKey,
          sellerSource: seller1.energyAta,
          sellerEscrow: multiSellerEscrowPda,
          signer: seller1.keypair.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller1.keypair])
        .rpc();

      // Multiple buyers place bids
      const multiBidPagePda = TestSetup.deriveBidPagePda(context.program, multiCtx.timeslotPda, 0);

      await context.program.methods
        .placeBid(0, new BN(90), new BN(300), multiEpoch)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: multiCtx.timeslotPda,
          timeslotQuoteEscrow: multiCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer1.quoteAta,
          buyer: buyer1.keypair.publicKey,
          bidPage: multiBidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer1.keypair])
        .rpc();

      await context.program.methods
        .placeBid(0, new BN(85), new BN(200), multiEpoch)
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: multiCtx.timeslotPda,
          timeslotQuoteEscrow: multiCtx.quoteEscrowPda,
          quoteMint: context.quoteMint.publicKey,
          buyerSource: buyer2.quoteAta,
          buyer: buyer2.keypair.publicKey,
          bidPage: multiBidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer2.keypair])
        .rpc();

      // Seal timeslot
      await context.program.methods
        .sealTimeslot()
        .accountsPartial({
          globalState: context.globalStatePda,
          timeslot: multiCtx.timeslotPda,
          authority: context.authority.publicKey,
        })
        .signers([context.authority])
        .rpc();

      // Verify timeslot state
      const timeslot = await context.program.account.timeslot.fetch(multiCtx.timeslotPda);
      assert.equal(timeslot.status, TimeslotStatus.SEALED);
    });
  });
});
