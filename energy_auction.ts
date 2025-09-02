import * as anchor from "@coral-xyz/anchor";
import { Program, AnchorError } from "@coral-xyz/anchor";
import { EnergyAuction } from "../target/types/energy_auction";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAssociatedTokenAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { assert } from "chai";

describe("energy_auction", () => {
  // provider & program
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.EnergyAuction as Program<EnergyAuction>;
  const authority = provider.wallet as anchor.Wallet; // test authority

  // test keypairs
  const seller = anchor.web3.Keypair.generate();
  const buyer = anchor.web3.Keypair.generate();
  const poorBuyer = anchor.web3.Keypair.generate();

  // mints (keypairs so we can pass them to createMint)
  const quoteMint = anchor.web3.Keypair.generate();
  const energyMint = anchor.web3.Keypair.generate();

  // ATAs
  let sellerEnergyAta: anchor.web3.PublicKey;
  let sellerQuoteAta: anchor.web3.PublicKey; // For receiving proceeds
  let buyerEnergyAta: anchor.web3.PublicKey; // For receiving energy
  let buyerQuoteAta: anchor.web3.PublicKey;
  let poorBuyerQuoteAta: anchor.web3.PublicKey;

  // PDAs we'll derive
  let globalStatePda: anchor.web3.PublicKey;
  let feeVaultPda: anchor.web3.PublicKey;

  // epoch BN for timeslot
  const epochTs = new anchor.BN(Date.now());

  // helper: airdrop lamports for keypair and confirm
  const airdropAndConfirm = async (pubkey: anchor.web3.PublicKey, lamports: number) => {
    const sig = await provider.connection.requestAirdrop(pubkey, lamports);
    const blockhash = await provider.connection.getLatestBlockhash();
    await provider.connection.confirmTransaction({
        signature: sig,
        ...blockhash
    }, "confirmed");
  };

  // helper: derive BidPage PDA from timeslot totalBids (reads on-chain)
  const deriveBidPagePda = async (timeslotPda: anchor.web3.PublicKey) => {
    const tsAcc = await program.account.timeslot.fetch(timeslotPda);
    // integer division by 150 (max bids per page)
    const pageIndexBN = tsAcc.totalBids.div(new anchor.BN(150));
    const pageIndexU32 = pageIndexBN.toNumber(); // Convert BN to number for the return value

    // The on-chain program expects a u32 for the page_index seed.
    // We must create a 4-byte buffer representing this u32 in little-endian format.
    const pageIndexBuffer = Buffer.alloc(4);
    pageIndexBuffer.writeUInt32LE(pageIndexU32, 0);

    const [pda] = anchor.web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("bid_page"),
        timeslotPda.toBuffer(),
        pageIndexBuffer, // Use the 4-byte u32 buffer
      ],
      program.programId
    );
    return { pda, pageIndex: pageIndexU32 };
  };


  before(async () => {
    // fund seller/buyer/poorBuyer
    await airdropAndConfirm(seller.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await airdropAndConfirm(buyer.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await airdropAndConfirm(poorBuyer.publicKey, 1 * anchor.web3.LAMPORTS_PER_SOL);

    // create mints
    await createMint(
      provider.connection,
      authority.payer,
      authority.publicKey,
      null,
      6, // quote mint decimals (USDC-like)
      quoteMint
    );

    await createMint(
      provider.connection,
      authority.payer,
      authority.publicKey,
      null,
      0, // energy mint decimals (kWh)
      energyMint
    );

    // create ATAs for seller/buyer/poorBuyer
    sellerEnergyAta = await createAssociatedTokenAccount(
      provider.connection,
      seller,
      energyMint.publicKey,
      seller.publicKey
    );
    // ATA for seller to receive USDC proceeds
    sellerQuoteAta = await createAssociatedTokenAccount(
      provider.connection,
      seller,
      quoteMint.publicKey,
      seller.publicKey
    );

    buyerQuoteAta = await createAssociatedTokenAccount(
      provider.connection,
      buyer,
      quoteMint.publicKey,
      buyer.publicKey
    );
    // ATA for buyer to receive energy tokens
    buyerEnergyAta = await createAssociatedTokenAccount(
      provider.connection,
      buyer,
      energyMint.publicKey,
      buyer.publicKey
    );

    poorBuyerQuoteAta = await createAssociatedTokenAccount(
      provider.connection,
      poorBuyer,
      quoteMint.publicKey,
      poorBuyer.publicKey
    );

    // mint initial balances
    await mintTo(
      provider.connection,
      authority.payer,
      energyMint.publicKey,
      sellerEnergyAta,
      authority.publicKey,
      1000 // 1000 kWh units (0 decimals)
    );

    await mintTo(
      provider.connection,
      authority.payer,
      quoteMint.publicKey,
      buyerQuoteAta,
      authority.publicKey,
      500_000 * 1_000_000 // 500,000 USDC (6 decimals)
    );

    await mintTo(
      provider.connection,
      authority.payer,
      quoteMint.publicKey,
      poorBuyerQuoteAta,
      authority.publicKey,
      1000 // 0.001 USDC (6-dec)
    );

    // derive global PDAs
    [globalStatePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("global_state")],
      program.programId
    );
    [feeVaultPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("fee_vault")],
      program.programId
    );
  });

  it("✅ Initializes the global state", async () => {
    const feeBps = 100; // 1%
    const version = 1;

    await program.methods
      .initGlobalState(feeBps, version)
      .accounts({
        globalState: globalStatePda,
        quoteMint: quoteMint.publicKey,
        feeVault: feeVaultPda,
        authority: authority.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .rpc();

    const state = await program.account.globalState.fetch(globalStatePda);
    assert.ok(state.authority.equals(authority.publicKey));
    assert.equal(state.feeBps, feeBps);
    assert.equal(state.version, version);
    assert.ok(state.quoteMint.equals(quoteMint.publicKey));
    assert.ok(state.feeVault.equals(feeVaultPda));
  });

  it("✅ Opens a new timeslot", async () => {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const lotSize = new anchor.BN(1); // 1 kWh per lot
    const priceTick = new anchor.BN(1_000_000); // $1.00 (6 dec)

    await program.methods
      .openTimeslot(epochTs, lotSize, priceTick)
      .accounts({
        globalState: globalStatePda,
        timeslot: timeslotPda,
        authority: authority.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();

    const slot = await program.account.timeslot.fetch(timeslotPda);
    assert.isTrue(slot.epochTs.eq(epochTs));
    assert.equal(slot.status, 1); // Open
    assert.isTrue(slot.lotSize.eq(lotSize));
    assert.isTrue(slot.priceTick.eq(priceTick));
    assert.isTrue(slot.totalSupply.eq(new anchor.BN(0)));
  });

  it("✅ Allows a seller to commit supply", async () => {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const [supplyPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("supply"), timeslotPda.toBuffer(), seller.publicKey.toBuffer()],
      program.programId
    );

    const [sellerEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("seller_escrow"), timeslotPda.toBuffer(), seller.publicKey.toBuffer()],
      program.programId
    );

    const quantity = new anchor.BN(100);
    const reservePrice = new anchor.BN(10_000_000); // $10.00 (6 dec)

    const sellerBefore = (await getAccount(provider.connection, sellerEnergyAta)).amount;

    await program.methods
      .commitSupply(epochTs, reservePrice, quantity)
      .accounts({
        globalState: globalStatePda,
        timeslot: timeslotPda,
        supply: supplyPda,
        energyMint: energyMint.publicKey,
        sellerSource: sellerEnergyAta,
        sellerEscrow: sellerEscrowPda,
        signer: seller.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([seller])
      .rpc();

    const supply = await program.account.supply.fetch(supplyPda);
    assert.ok(supply.supplier.equals(seller.publicKey));
    assert.ok(supply.timeslot.equals(timeslotPda));
    assert.isTrue(supply.amount.eq(quantity));
    assert.isTrue(supply.reservePrice.eq(reservePrice));

    const sellerAfter = (await getAccount(provider.connection, sellerEnergyAta)).amount;
    const escrowBalance = (await getAccount(provider.connection, sellerEscrowPda)).amount;
    assert.equal(Number(escrowBalance), Number(quantity));
    assert.equal(Number(sellerBefore) - Number(sellerAfter), Number(quantity));

    const slot = await program.account.timeslot.fetch(timeslotPda);
    assert.isTrue(slot.totalSupply.eq(quantity));
  });

  it("✅ Allows a buyer to place a bid", async () => {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const [timeslotQuoteEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
      program.programId
    );

    const { pda: bidPagePda, pageIndex } = await deriveBidPagePda(timeslotPda);

    const price = new anchor.BN(12_000_000); // $12.00
    const quantity = new anchor.BN(50);
    const timestamp = new anchor.BN(Date.now());
    const expectedEscrowAmount = price.mul(quantity);

    const buyerBefore = (await getAccount(provider.connection, buyerQuoteAta)).amount;

    await program.methods
      .placeBid(pageIndex, price, quantity, timestamp)
      .accounts({
        globalState: globalStatePda,
        timeslot: timeslotPda,
        timeslotQuoteEscrow: timeslotQuoteEscrow,
        quoteMint: quoteMint.publicKey,
        buyerSource: buyerQuoteAta,
        buyer: buyer.publicKey,
        bidPage: bidPagePda,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([buyer])
      .rpc();

    const buyerAfter = (await getAccount(provider.connection, buyerQuoteAta)).amount;
    const escrowAcc = await getAccount(provider.connection, timeslotQuoteEscrow);
    assert.equal(escrowAcc.amount.toString(), expectedEscrowAmount.toString(), "escrow credited");
    assert.equal(
      (buyerBefore - buyerAfter).toString(),
      expectedEscrowAmount.toString(),
      "buyer debited"
    );

    const page = await program.account.bidPage.fetch(bidPagePda);
    assert.equal(page.bids.length, 1);
    const bid = page.bids[0];
    assert.ok(bid.owner.equals(buyer.publicKey));
    assert.isTrue(bid.price.eq(price));
    assert.isTrue(bid.quantity.eq(quantity));
    assert.equal(bid.status, 0); // Active

    const slot = await program.account.timeslot.fetch(timeslotPda);
    assert.isTrue(slot.totalBids.eq(quantity));
  });

  it("🚫 Fails to place a bid with invalid price tick", async () => {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const [timeslotQuoteEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
      program.programId
    );

    const { pda: bidPagePda, pageIndex } = await deriveBidPagePda(timeslotPda);

    const price = new anchor.BN(12_500_000); // $12.50, but tick is $1.00
    const quantity = new anchor.BN(10);
    const timestamp = new anchor.BN(Date.now());

    try {
      await program.methods
        .placeBid(pageIndex, price, quantity, timestamp)
        .accounts({
          globalState: globalStatePda,
          timeslot: timeslotPda,
          timeslotQuoteEscrow,
          quoteMint: quoteMint.publicKey,
          buyerSource: buyerQuoteAta,
          buyer: buyer.publicKey,
          bidPage: bidPagePda,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer])
        .rpc();
      assert.fail("Expected invalid price tick to fail");
    } catch (err) {
      assert.instanceOf(err, AnchorError);
      assert.equal((err as AnchorError).error.errorCode.code, "ConstraintViolation");
    }
  });

  it("✅ Seals the timeslot", async () => {
    const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    await program.methods
      .sealTimeslot()
      .accounts({
        globalState: globalStatePda,
        timeslot: timeslotPda,
        authority: authority.publicKey,
      })
      .rpc();

    const slot = await program.account.timeslot.fetch(timeslotPda);
    assert.equal(slot.status, 2); // Sealed
  });

  // --- NEW SETTLEMENT TESTS ---
  describe("Settlement Flow", () => {
    const clearingPrice = new anchor.BN(11_000_000); // $11.00
    const totalSoldQuantity = new anchor.BN(50); // Buyer wins all 50 they bid for

    it("✅ Settles the timeslot", async () => {
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
      
      await program.methods
        .settleTimeslot(clearingPrice, totalSoldQuantity)
        .accounts({
          globalState: globalStatePda,
          timeslot: timeslotPda,
          authority: authority.publicKey,
        })
        .rpc();

      const slot = await program.account.timeslot.fetch(timeslotPda);
      assert.equal(slot.status, 3, "Timeslot should be Settled");
      assert.isTrue(slot.clearingPrice.eq(clearingPrice), "Clearing price should be set");
      assert.isTrue(slot.totalSoldQuantity.eq(totalSoldQuantity), "Total sold quantity should be set");
    });

    it("✅ Creates a fill receipt for the winning buyer", async () => {
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
      const [fillReceiptPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("fill_receipt"), timeslotPda.toBuffer(), buyer.publicKey.toBuffer()],
        program.programId
      );

      const wonQuantity = new anchor.BN(50);

      await program.methods
        .createFillReceipt(wonQuantity)
        .accounts({
          globalState: globalStatePda,
          timeslot: timeslotPda,
          buyer: buyer.publicKey,
          fillReceipt: fillReceiptPda,
          authority: authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      const receipt = await program.account.fillReceipt.fetch(fillReceiptPda);
      assert.ok(receipt.buyer.equals(buyer.publicKey));
      assert.ok(receipt.timeslot.equals(timeslotPda));
      assert.isTrue(receipt.quantity.eq(wonQuantity));
      assert.isTrue(receipt.clearingPrice.eq(clearingPrice));
      assert.isFalse(receipt.redeemed);
    });

    it("✅ Allows seller to withdraw proceeds", async () => {
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
      const [supplyPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("supply"), timeslotPda.toBuffer(), seller.publicKey.toBuffer()],
        program.programId
      );
      const [timeslotQuoteEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
        program.programId
      );

      const globalState = await program.account.globalState.fetch(globalStatePda);
      const grossProceeds = totalSoldQuantity.mul(clearingPrice);
      const fee = grossProceeds.mul(new anchor.BN(globalState.feeBps)).div(new anchor.BN(10000));
      const expectedNetProceeds = grossProceeds.sub(fee);

      const sellerQuoteBefore = (await getAccount(provider.connection, sellerQuoteAta)).amount;
      const feeVaultBefore = (await getAccount(provider.connection, feeVaultPda)).amount;

      await program.methods
        .withdrawProceeds()
        .accounts({
          globalState: globalStatePda,
          timeslot: timeslotPda,
          supply: supplyPda,
          timeslotQuoteEscrow: timeslotQuoteEscrow,
          feeVault: feeVaultPda,
          sellerProceedsAta: sellerQuoteAta,
          seller: seller.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([seller])
        .rpc();

      const sellerQuoteAfter = (await getAccount(provider.connection, sellerQuoteAta)).amount;
      const feeVaultAfter = (await getAccount(provider.connection, feeVaultPda)).amount;
      
      assert.equal(
        (sellerQuoteAfter - sellerQuoteBefore).toString(),
        expectedNetProceeds.toString(),
        "Seller should receive net proceeds"
      );
      assert.equal(
        (feeVaultAfter - feeVaultBefore).toString(),
        fee.toString(),
        "Fee vault should receive the protocol fee"
      );

      const updatedSupply = await program.account.supply.fetch(supplyPda);
      assert.isTrue(updatedSupply.claimed, "Supply should be marked as claimed");
    });

    it("✅ Allows buyer to redeem energy and get a refund", async () => {
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
      const [fillReceiptPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("fill_receipt"), timeslotPda.toBuffer(), buyer.publicKey.toBuffer()],
        program.programId
      );
      const [timeslotQuoteEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
        program.programId
      );
      const [sellerEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("seller_escrow"), timeslotPda.toBuffer(), seller.publicKey.toBuffer()],
        program.programId
      );

      // Calculate expected refund
      const bidPrice = new anchor.BN(12_000_000);
      const bidQuantity = new anchor.BN(50);
      const totalBidAmountEscrowed = bidPrice.mul(bidQuantity);

      const receipt = await program.account.fillReceipt.fetch(fillReceiptPda);
      const actualCost = receipt.quantity.mul(receipt.clearingPrice);
      const expectedRefund = totalBidAmountEscrowed.sub(actualCost);

      const buyerQuoteBefore = (await getAccount(provider.connection, buyerQuoteAta)).amount;
      const buyerEnergyBefore = (await getAccount(provider.connection, buyerEnergyAta)).amount;

      await program.methods
        .redeemEnergyAndRefund(totalBidAmountEscrowed)
        .accounts({
          timeslot: timeslotPda,
          fillReceipt: fillReceiptPda,
          timeslotQuoteEscrow: timeslotQuoteEscrow,
          buyerQuoteAta: buyerQuoteAta,
          buyerEnergyAta: buyerEnergyAta,
          sellerEscrow: sellerEscrowPda, // NOTE: Assuming one seller for simplicity
          buyer: buyer.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([buyer])
        .rpc();

      const buyerQuoteAfter = (await getAccount(provider.connection, buyerQuoteAta)).amount;
      const buyerEnergyAfter = (await getAccount(provider.connection, buyerEnergyAta)).amount;

      assert.equal(
        (buyerQuoteAfter - buyerQuoteBefore).toString(),
        expectedRefund.toString(),
        "Buyer should receive a refund"
      );
      assert.equal(
        (buyerEnergyAfter - buyerEnergyBefore).toString(),
        receipt.quantity.toString(),
        "Buyer should receive energy tokens"
      );

      const updatedReceipt = await program.account.fillReceipt.fetch(fillReceiptPda);
      assert.isTrue(updatedReceipt.redeemed, "Receipt should be marked as redeemed");
    });
  });

  // --- NEGATIVE TESTS (UNCHANGED) ---
  describe("Negative Paths", () => {
    it("🚫 Fails to place a bid after sealing", async () => {
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
  
      const [timeslotQuoteEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("quote_escrow"), timeslotPda.toBuffer()],
        program.programId
      );
  
      const { pda: bidPagePda, pageIndex } = await deriveBidPagePda(timeslotPda);
  
      const price = new anchor.BN(13_000_000);
      const quantity = new anchor.BN(1);
      const timestamp = new anchor.BN(Date.now());
  
      try {
        await program.methods
          .placeBid(pageIndex, price, quantity, timestamp)
          .accounts({
            globalState: globalStatePda,
            timeslot: timeslotPda,
            timeslotQuoteEscrow,
            quoteMint: quoteMint.publicKey,
            buyerSource: buyerQuoteAta,
            buyer: buyer.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([buyer])
          .rpc();
        assert.fail("Expected bidding after sealing to fail");
      } catch (err) {
        assert.instanceOf(err, AnchorError);
        assert.equal((err as AnchorError).error.errorCode.code, "InvalidTimeslot");
      }
    });
  
    it("🚫 Fails to commit supply to a sealed timeslot", async () => {
      const anotherSeller = anchor.web3.Keypair.generate();
      await airdropAndConfirm(anotherSeller.publicKey, 1 * anchor.web3.LAMPORTS_PER_SOL);
  
      const anotherSellerAta = await createAssociatedTokenAccount(
        provider.connection,
        anotherSeller,
        energyMint.publicKey,
        anotherSeller.publicKey
      );
  
      await mintTo(
        provider.connection,
        authority.payer,
        energyMint.publicKey,
        anotherSellerAta,
        authority.publicKey,
        100
      );
  
      const [timeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), epochTs.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
  
      const [supplyPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("supply"), timeslotPda.toBuffer(), anotherSeller.publicKey.toBuffer()],
        program.programId
      );
  
      const [sellerEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("seller_escrow"), timeslotPda.toBuffer(), anotherSeller.publicKey.toBuffer()],
        program.programId
      );
  
      try {
        await program.methods
          .commitSupply(epochTs, new anchor.BN(1_000_000), new anchor.BN(1))
          .accounts({
            globalState: globalStatePda,
            timeslot: timeslotPda,
            supply: supplyPda,
            energyMint: energyMint.publicKey,
            sellerSource: anotherSellerAta,
            sellerEscrow: sellerEscrowPda,
            signer: anotherSeller.publicKey,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([anotherSeller])
          .rpc();
        assert.fail("Expected commit after sealing to fail");
      } catch (err) {
        assert.instanceOf(err, AnchorError);
        assert.equal((err as AnchorError).error.errorCode.code, "InvalidTimeslot");
      }
    });
  
    it("🚫 Fails to place a bid with insufficient buyer balance", async () => {
      // create new timeslot for this negative test
      const newEpoch = new anchor.BN(Date.now() + 10_000);
      const [newTimeslotPda] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("timeslot"), newEpoch.toArrayLike(Buffer, "le", 8)],
        program.programId
      );
  
      await program.methods
        .openTimeslot(newEpoch, new anchor.BN(1), new anchor.BN(1_000_000))
        .accounts({
          globalState: globalStatePda,
          timeslot: newTimeslotPda,
          authority: authority.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();
  
      const [newTimeslotEscrow] = anchor.web3.PublicKey.findProgramAddressSync(
        [Buffer.from("quote_escrow"), newTimeslotPda.toBuffer()],
        program.programId
      );
  
      const { pda: bidPagePda, pageIndex } = await deriveBidPagePda(newTimeslotPda);
  
      const price = new anchor.BN(1_000_000); // $1.00
      const quantity = new anchor.BN(10_000); // requires 10,000 USDC, poorBuyer has 0.001
      const timestamp = new anchor.BN(Date.now());
  
      try {
        await program.methods
          .placeBid(pageIndex, price, quantity, timestamp)
          .accounts({
            globalState: globalStatePda,
            timeslot: newTimeslotPda,
            timeslotQuoteEscrow: newTimeslotEscrow,
            quoteMint: quoteMint.publicKey,
            buyerSource: poorBuyerQuoteAta,
            buyer: poorBuyer.publicKey,
            bidPage: bidPagePda,
            systemProgram: anchor.web3.SystemProgram.programId,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([poorBuyer])
          .rpc();
        assert.fail("Expected bid to fail due to insufficient funds");
      } catch (err) {
        // The error from an insufficient funds transfer via CPI can be generic.
        // We check for common RPC error messages or the specific token program error.
        const errorString = err.toString();
        const isExpectedError =
          errorString.includes("failed to send transaction") ||
          errorString.includes("Simulation failed") ||
          errorString.includes("insufficient funds") ||
          errorString.includes("custom program error: 0x1"); // SPL Token program's InsufficientFunds error
  
        assert.isTrue(isExpectedError, `Unexpected error received: ${errorString}`);
      }
    });
  });
});
