// node compute_mint_auth.js
import { PublicKey } from "@solana/web3.js";

const PROGRAM_ID = new PublicKey("5V4D1b9wrjuJC3aAtNbayVgMYt5879w2rL2k5UoQGTvM"); // your program id here

(async () => {
  const [mintAuthPda, bump] = await PublicKey.findProgramAddress(
    [Buffer.from("mint_auth")],
    PROGRAM_ID
  );
  console.log("MINT_AUTH_PDA:", mintAuthPda.toBase58());
  console.log("MINT_AUTH_BUMP:", bump);
})();
