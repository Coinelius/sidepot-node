//! A light client's whole job, without a node's wallet: an address from a key, and a signed transfer or
//! withdrawal request built from the coins a server lists. The node attaches the Utreexo proof when it takes the
//! transaction (`submit_transaction`), so nothing here follows the chain.
//!
//!   light_client address <seed hex, 32 bytes>
//!   light_client transfer <seed hex> <utxos.json> <to address> <value sats> <fee sats>
//!   light_client withdraw <seed hex> <utxos.json> <main address> <value sats> <main fee sats> <fee sats>
//!   light_client fund <seed hex> <utxos.json> <account hex> <value sats> <fee sats>
//!
//! `fund` moves coins into a game account (its 32-byte key, 64 hex digits) with a game deposit output, which
//! a node takes only once coin deposits are active on its network.
//!
//! `utxos.json` is the result of the node's `list_utxos`. The signed transaction is printed as the JSON
//! `submit_transaction` takes. Change returns to the sender's address.

use anyhow::{Context as _, anyhow, bail};
use bitcoin::Amount;
use thunder_types::{
    Address, AuthorizedTransaction, GetValue as _, Output, OutputContent,
    PointedOutput, Transaction,
    authorization::{self, SigningKey, VerifyingKey, get_address},
    hashes::hash,
};

/// The signing key of a 32-byte seed: the seed hashed onto the scalar field, so every seed gives a key.
fn signing_key(seed_hex: &str) -> anyhow::Result<SigningKey> {
    let seed: [u8; 32] = const_hex::decode(seed_hex)?
        .try_into()
        .map_err(|_| anyhow!("the seed is 32 bytes of hex"))?;
    let wide: [u8; 64] = {
        let mut out = [0; 64];
        blake3::Hasher::new_derive_key("sidepot light client signing key")
            .update(&seed)
            .finalize_xof()
            .fill(&mut out);
        out
    };
    let scalar = curve25519_dalek::Scalar::from_bytes_mod_order_wide(&wide);
    SigningKey::deserialize(scalar.as_bytes()).map_err(|e| anyhow!("{e}"))
}

fn sats(text: &str) -> anyhow::Result<Amount> {
    Ok(Amount::from_sat(text.parse()?))
}

/// Spends the key's coins, largest first, into `payment` plus change, and signs once per input.
fn build(
    key: &SigningKey,
    utxos_path: &str,
    to: Option<Address>,
    payment: OutputContent,
    fee: Amount,
) -> anyhow::Result<AuthorizedTransaction> {
    let me = get_address(VerifyingKey::from(key));
    let utxos: Vec<PointedOutput> =
        serde_json::from_str(&std::fs::read_to_string(utxos_path)?)
            .context("utxos.json is not a list_utxos result")?;
    let mut mine: Vec<_> = utxos
        .into_iter()
        .filter(|utxo| utxo.output.address == me && utxo.output.content.is_value())
        .collect();
    mine.sort_by_key(|utxo| std::cmp::Reverse(utxo.output.get_value()));
    let need = payment.get_value() + fee;
    let (mut total, mut inputs) = (Amount::ZERO, Vec::new());
    for utxo in &mine {
        if total >= need {
            break;
        }
        total += utxo.output.get_value();
        inputs.push((utxo.outpoint, hash(utxo)));
    }
    if total < need {
        bail!("{me} holds {total}, and this needs {need}");
    }
    // A game deposit is not a coin: its address is unused and must be all zeros.
    let payment_address = if payment.is_game_deposit() {
        Address::ALL_ZEROS
    } else {
        to.unwrap_or(me)
    };
    let mut outputs = vec![Output {
        address: payment_address,
        content: payment,
    }];
    if total > need {
        outputs.push(Output {
            address: me,
            content: OutputContent::Value(total - need),
        });
    }
    let transaction = Transaction {
        inputs: inputs.into(),
        outputs: outputs.into(),
        ..Default::default()
    };
    let signers: Vec<_> =
        transaction.inputs.iter().map(|_| (me, key)).collect();
    Ok(authorization::authorize(rand::rng(), &signers, transaction)?)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["address", seed] => {
            let key = signing_key(seed)?;
            println!("{}", get_address(VerifyingKey::from(&key)));
        }
        ["transfer", seed, utxos, to, value, fee] => {
            let key = signing_key(seed)?;
            let to: Address = to.parse()?;
            let payment = OutputContent::Value(sats(value)?);
            let tx = build(&key, utxos, Some(to), payment, sats(fee)?)?;
            println!("{}", serde_json::to_string(&tx)?);
        }
        ["withdraw", seed, utxos, main_address, value, main_fee, fee] => {
            let key = signing_key(seed)?;
            let payment = OutputContent::Withdrawal {
                value: sats(value)?,
                main_fee: sats(main_fee)?,
                main_address: main_address.parse()?,
            };
            let tx = build(&key, utxos, None, payment, sats(fee)?)?;
            println!("{}", serde_json::to_string(&tx)?);
        }
        ["fund", seed, utxos, account, value, fee] => {
            let key = signing_key(seed)?;
            let mut account_key = [0; 32];
            if account.len() != 64
                || const_hex::decode_to_slice(account, &mut account_key)
                    .is_err()
            {
                bail!("the account is 64 hex digits");
            }
            let payment = OutputContent::GameDeposit {
                value: sats(value)?,
                account: account_key,
            };
            let tx = build(&key, utxos, None, payment, sats(fee)?)?;
            println!("{}", serde_json::to_string(&tx)?);
        }
        _ => bail!("usage: light_client address|transfer|withdraw|fund … (see the top of this file)"),
    }
    Ok(())
}
