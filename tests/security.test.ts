import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { assert } from "chai";
import { TestSetup, TestContext, TestAccount } from "./test-setup";

describe("Security Tests - Access Control & Safety", () => {
  let context: TestContext;
  let seller: TestAccount;
  let buyer: TestAccount;
  let attacker: TestAccount;

  before(async () => {
    context = await TestSetup.initializeTestContext();
    seller = await TestSetup.createTestAccount(context, 1000, 0);
    buyer = await TestSetup.createTestAccount(context, 0, 1_000_000 * 1_000_000);
    attacker = await TestSetup.createTestAccount(context, 500, 100_000 * 1_000_000);

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

  describe("Authority Access Controls", () => {
    it(" Prevents unauthorized timeslot opening", async () => {
      const fakeEpoch = new BN(1000);
      const timeslotPda = TestSetup.deriveTimeslotPdas(context.program, fakeEpoch).timeslotPda;

      await TestSetup.expectSpecificError(
        context.program.methods
          .openTimeslot(fakeEpoch, new BN(1), new BN(1_000_000))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotPda,
            authority: attacker.keypair.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
          })
          .signers([attacker.keypair])
          .rpc(),
        "InvalidAuthority"
      );
    });

    it(" Validates signer requirements", async () => {
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

      const { supplyPda, sellerEscrowPda } = TestSetup.deriveSupplyPdas(
        context.program,
        timeslotCtx.timeslotPda,
        seller.keypair.publicKey
      );

      await TestSetup.expectTransactionFailure(
        context.program.methods
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
          .rpc(), // Missing seller.keypair from signers
        "signature"
      );
    });
  });

  describe("Numerical Safety", () => {
    it(" Prevents overflow in calculations", async () => {
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

      const bidPagePda = TestSetup.deriveBidPagePda(context.program, timeslotCtx.timeslotPda, 0);

      await TestSetup.expectSpecificError(
        context.program.methods
          .placeBid(0, new BN("18446744073709551615"), new BN("18446744073709551615"), new BN(Date.now()))
          .accountsPartial({
            globalState: context.globalStatePda,
            timeslot: timeslotCtx.timeslotPda,
            timeslotQuoteEscrow: timeslotCtx.quoteEscrowPda,
            bidPage: bidPagePda,
            buyer: buyer.keypair.publicKey,
            buyerSource: buyer.quoteAta,
            quoteMint: context.quoteMint.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer.keypair])
          .rpc(),
        "MathError"
      );
    });

    it(" Validates zero quantity constraints", async () => {
      const epoch = new BN(Date.now() + 4000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accounts({
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

      await TestSetup.expectSpecificError(
        context.program.methods
          .commitSupply(epoch, new BN(5_000_000), new BN(0))
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
          .rpc(),
        "ConstraintViolation"
      );
    });
  });

  describe("State Manipulation Prevention", () => {
    it(" Prevents double supply commitment", async () => {
      const epoch = new BN(Date.now() + 5000);
      const timeslotCtx = TestSetup.deriveTimeslotPdas(context.program, epoch);

      await context.program.methods
        .openTimeslot(epoch, new BN(1), new BN(1_000_000))
        .accounts({
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

      // First commitment
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

      // Second commitment should fail
      await TestSetup.expectSpecificError(
        context.program.methods
          .commitSupply(epoch, new BN(6_000_000), new BN(50))
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
          .rpc(),
        "AccountAlreadyInUse"
      );
    });
  });
});
