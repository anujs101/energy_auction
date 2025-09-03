import * as anchor from "@coral-xyz/anchor";
import { BN } from "@coral-xyz/anchor";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { assert } from "chai";
import { TestSetup, TestContext, TestAccount, ProposalStatus, Vote } from "./test-setup";

describe("Governance Tests - DAO Functionality", () => {
  let context: TestContext;
  let councilMember: TestAccount;
  let proposer: TestAccount;
  let voter: TestAccount;

  before(async () => {
    context = await TestSetup.initializeTestContext();
    councilMember = await TestSetup.createTestAccount(context, 0, 0);
    proposer = await TestSetup.createTestAccount(context, 0, 0);
    voter = await TestSetup.createTestAccount(context, 0, 0);

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

  describe("Proposal Creation", () => {
    it("✅ Creates parameter change proposal", async () => {
      const proposalId = new BN(1);
      const { proposalPda } = TestSetup.deriveGovernancePdas(context.program, proposalId);

      await context.program.methods
        .proposeParameterChange(
          proposalId,
          { feeBps: {} },
          new BN(200),
          TestSetup.createDescriptionBuffer("Increase fee to 2%")
        )
        .accountsPartial({
          globalState: context.globalStatePda,
          proposal: proposalPda,
          proposer: context.authority.publicKey,
          proposerStake: context.authorityQuoteAta,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      const proposal = await context.program.account.governanceProposal.fetch(proposalPda);
      assert.ok(proposal.proposer.equals(context.authority.publicKey));
      assert.equal(proposal.status, ProposalStatus.ACTIVE);
    });

    it("🚫 Fails unauthorized proposal creation", async () => {
      const proposalId = new BN(2);
      const { proposalPda } = TestSetup.deriveGovernancePdas(context.program, proposalId);

      await TestSetup.expectSpecificError(
        context.program.methods
          .proposeParameterChange(
            proposalId,
            { feeBps: {} },
            new BN(300),
            TestSetup.createDescriptionBuffer("Unauthorized proposal")
          )
          .accountsPartial({
            globalState: context.globalStatePda,
            proposal: proposalPda,
            proposer: proposer.keypair.publicKey,
            proposerStake: proposer.quoteAta,
            systemProgram: anchor.web3.SystemProgram.programId,
            clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
          })
          .signers([proposer.keypair])
          .rpc(),
        "InsufficientStake"
      );
    });
  });

  describe("Voting Mechanism", () => {
    let proposalPda: anchor.web3.PublicKey;

    before(async () => {
      const proposalId = new BN(3);
      ({ proposalPda } = TestSetup.deriveGovernancePdas(context.program, proposalId));

      await context.program.methods
        .proposeParameterChange(
          proposalId,
          { feeBps: {} },
          new BN(300),
          TestSetup.createDescriptionBuffer("Emergency proposal")
        )
        .accountsPartial({
          globalState: context.globalStatePda,
          proposal: proposalPda,
          proposer: context.authority.publicKey,
          proposerStake: context.authorityQuoteAta,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();
    });

    it("✅ Casts vote successfully", async () => {
      const { voteRecordPda } = TestSetup.deriveGovernancePdas(context.program, new BN(3), context.authority.publicKey);

      await context.program.methods
        .voteOnProposal({ for: {} }, new BN(1000))
        .accountsPartial({
          globalState: context.globalStatePda,
          proposal: proposalPda,
          voteRecord: voteRecordPda,
          voter: context.authority.publicKey,
          voterStake: context.authorityQuoteAta,
          systemProgram: anchor.web3.SystemProgram.programId,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      const voteRecord = await context.program.account.voteRecord.fetch(voteRecordPda!);
      assert.equal(Object.keys(voteRecord.vote)[0], 'for');
      assert.isTrue(voteRecord.votingPower.eq(new BN(1000)));
    });

    it("✅ Executes approved proposal", async () => {
      // Wait for voting to complete (voting period is 1s for emergency)
      await new Promise(resolve => setTimeout(resolve, 1000));

      await context.program.methods
        .executeProposal()
        .accountsPartial({
          globalState: context.globalStatePda,
          proposal: proposalPda,
          authority: context.authority.publicKey, // Authority required to execute
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      const globalState = await context.program.account.globalState.fetch(context.globalStatePda);
      assert.equal(globalState.feeBps, 300);
    });
  });
});
