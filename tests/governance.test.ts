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
          TestSetup.createDescriptionBuffer("Unauthorized proposal")
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
      
      // Use proposal_id for PDA derivation (matches smart contract)
      const proposalId = new BN(1); // Fixed proposal ID
      const { proposalPda, voteRecordPda } = TestSetup.deriveGovernancePdas(context.program, proposalId, context.authority.publicKey);

      // Check if proposal already exists
      let proposalExists = false;
      try {
        await context.program.account.governanceProposal.fetch(proposalPda);
        proposalExists = true;
      } catch (error) {
        // Proposal doesn't exist, create it
      }

      if (!proposalExists) {
        // Create proposal
        await context.program.methods
          .proposeParameterChange(
            proposalId,
            { feeBps: {} },
            new BN(200),
            TestSetup.createDescriptionBuffer("Test proposal for voting")
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
      }

      // Check if vote record already exists
      let voteExists = false;
      try {
        await context.program.account.voteRecord.fetch(voteRecordPda!);
        voteExists = true;
      } catch (error) {
        // Vote record doesn't exist, create it
      }

      if (!voteExists) {
        // Vote on the proposal using the original PDA
        await context.program.methods
          .voteOnProposal({ for: {} }, new BN(1000))
          .accountsPartial({
            globalState: context.globalStatePda,
            proposal: proposalPda, // Use original PDA for voting
            voteRecord: voteRecordPda,
            voter: context.authority.publicKey,
            voterStake: context.authorityQuoteAta,
            systemProgram: anchor.web3.SystemProgram.programId,
            clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
          })
          .signers([context.authority])
          .rpc();
      }

      const voteRecord = await context.program.account.voteRecord.fetch(voteRecordPda!);
      assert.equal(Object.keys(voteRecord.vote)[0], 'for'); // Vote.FOR
      assert.isTrue(voteRecord.votingPower.eq(new BN(1000)));
    });

    it("✅ Executes approved proposal", async () => {
      
      // Use proposal_id for PDA derivation (matches smart contract)
      const proposalId = new BN(1); // Same proposal ID as voting test
      const { proposalPda } = TestSetup.deriveGovernancePdas(context.program, proposalId);

      // Check if proposal already exists
      let proposalExists = false;
      try {
        await context.program.account.governanceProposal.fetch(proposalPda);
        proposalExists = true;
      } catch (error) {
        // Proposal doesn't exist, create it
      }

      if (!proposalExists) {
        // Create proposal
        await context.program.methods
          .proposeParameterChange(
            proposalId,
            { emergencyParameterChange: {} },
            new BN(250), // Change fee to 250 bps to match test expectation
            TestSetup.createDescriptionBuffer("Emergency test proposal")
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

        // Vote on the proposal to pass it
        const { voteRecordPda } = TestSetup.deriveGovernancePdas(context.program, proposalId, context.authority.publicKey);
        
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
      }

      // Wait for emergency proposal voting deadline to pass (1 minute)
      await new Promise(resolve => setTimeout(resolve, 2000)); // 2 second delay for test timing
      
      // Check proposal status before execution
      const proposal = await context.program.account.governanceProposal.fetch(proposalPda);
      console.log("Proposal status before execution:", proposal.status);
      console.log("Proposal type:", proposal.proposalType);
      
      // Execute the proposal using the same PDA used for creation
      await context.program.methods
        .executeProposal()
        .accountsPartial({
          globalState: context.globalStatePda,
          proposal: proposalPda, // Use same PDA as creation
          authority: context.authority.publicKey,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([context.authority])
        .rpc();

      // Verify proposal was executed by checking global state change
      const state = await context.program.account.globalState.fetch(context.globalStatePda);
      assert.equal(state.feeBps, 250);
    });
  });
});
