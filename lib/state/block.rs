//! Connect and disconnect blocks

use sneed::{RoTxn, RwTxn, db::error::Error as DbError};

use crate::{
    state::{Error, PrevalidatedBlock, State, error},
    types::{
        AccumulatorDiff, AmountOverflowError, Body, FilledTransaction,
        GetAddress as _, GetValue as _, Header, InPoint, MerkleRoot, OutPoint,
        OutPointKey, Output, PointedOutput, SpentOutput, UtreexoNodeHash,
        authorization::{self, BatchVerificationContext},
    },
};

/// Prevalidate a block: compute and verify all read-only checks and
/// prepare data needed for fast connection.
/// Sidepot: where a body may hold game deposit outputs. Each such output is checked on its own by
/// [`State::validate_filled_transaction`].
fn check_game_deposits(
    body: &Body,
    game_deposits_active: bool,
) -> Result<(), Error> {
    if body
        .coinbase
        .outputs
        .iter()
        .any(|output| output.content.is_game_deposit())
    {
        return Err(Error::GameDepositInCoinbase);
    }
    if !game_deposits_active
        && body.transactions.iter().any(|tx| tx.has_game_deposit())
    {
        return Err(Error::GameDepositsNotActive);
    }
    Ok(())
}

pub fn prevalidate(
    batch_verification_ctxt: &BatchVerificationContext,
    state: &State,
    rotxn: &RoTxn,
    header: &Header,
    body: &Body,
    payouts: &[Output],
    game_deposits_active: bool,
) -> Result<PrevalidatedBlock, Error> {
    let tip_hash = state.try_get_tip(rotxn)?;
    if header.prev_side_hash != tip_hash {
        let err = error::InvalidHeader::PrevSideHash {
            expected: tip_hash,
            received: header.prev_side_hash,
        };
        return Err(Error::InvalidHeader(err));
    };
    // Sidepot: a coinbase never holds a game deposit, and a transaction may only once they are active for this
    // block (the caller decides that from the L1 block the header names).
    let () = check_game_deposits(body, game_deposits_active)?;
    let next_height = state.try_get_height(rotxn)?.map_or(0, |h| h + 1);
    if body.authorizations.len() > State::body_sigops_limit(next_height) {
        return Err(Error::TooManySigops);
    }
    let body_size =
        borsh::object_length(&body).map_err(Error::BorshSerialize)?;
    if body_size > State::body_size_limit(next_height) {
        return Err(Error::BodyTooLarge);
    }

    let mut accumulator = state
        .utreexo_accumulator
        .try_get(rotxn, &())
        .map_err(DbError::from)?
        .unwrap_or_default();

    // gather and verify transactions
    let mut total_inputs: usize = 0;
    let mut total_outputs: usize = 0;
    for tx in &body.transactions {
        total_inputs += tx.inputs.len();
        total_outputs += tx.outputs.len();
    }
    let mut all_input_keys: Vec<OutPointKey> = Vec::with_capacity(total_inputs);
    // Accumulator diff from txs. The bool value is true for insertions, false
    // for deletions.
    let mut accumulator_diff_txs =
        Vec::with_capacity(total_inputs + total_outputs);
    let mut filled_transactions: Vec<FilledTransaction> =
        Vec::with_capacity(body.transactions.len());
    let mut total_fees = bitcoin::Amount::ZERO;
    for transaction in &body.transactions {
        let txid = transaction.txid();
        let mut spent_utxos = Vec::with_capacity(transaction.inputs.len());
        let mut spent_utxo_hashes =
            Vec::<UtreexoNodeHash>::with_capacity(transaction.inputs.len());
        for (outpoint, utxo_hash) in &transaction.inputs {
            let key = OutPointKey::from(outpoint);
            let spent_output =
                state.utxos.try_get(rotxn, &key)?.ok_or(error::NoUtxo {
                    outpoint: *outpoint,
                })?;
            all_input_keys.push(OutPointKey::from(outpoint));
            spent_utxos.push(spent_output);
            spent_utxo_hashes.push(utxo_hash.into());
            accumulator_diff_txs.push((false, utxo_hash.into()));
        }
        for (vout, output) in transaction.outputs.iter().enumerate() {
            // Sidepot: a game deposit is not a coin, so the accumulator never holds it.
            if output.content.is_game_deposit() {
                continue;
            }
            let outpoint = OutPoint::Regular {
                txid,
                vout: vout as u32,
            };
            let pointed_output = PointedOutput {
                outpoint,
                output: output.clone(),
            };
            accumulator_diff_txs.push((true, (&pointed_output).into()));
        }
        if !accumulator.verify(&transaction.proof, &spent_utxo_hashes)? {
            return Err(Error::UtreexoProofFailed { txid });
        }
        let filled_tx = FilledTransaction {
            spent_utxos,
            transaction: transaction.clone(),
        };
        total_fees = total_fees
            .checked_add(state.validate_filled_transaction(&filled_tx)?)
            .ok_or(AmountOverflowError)?;
        filled_transactions.push(filled_tx);
    }
    let computed_merkle_root = Body::compute_merkle_root(
        &body.coinbase,
        filled_transactions.as_slice(),
    )?;
    if computed_merkle_root != header.merkle_root {
        let err = Error::InvalidBody {
            expected: header.merkle_root,
            computed: computed_merkle_root,
        };
        return Err(err);
    }
    {
        use rayon::prelude::ParallelSliceMut;
        all_input_keys.par_sort_unstable();
        if all_input_keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::UtxoDoubleSpent);
        }
    }
    let coinbase_txid = header.compute_coinbase_txid();
    let mut coinbase_value = bitcoin::Amount::ZERO;
    let mut accumulator_diff = AccumulatorDiff::with_capacity(
        body.coinbase.outputs.len() + accumulator_diff_txs.len(),
    );
    for (vout, output) in body.coinbase.outputs.iter().enumerate() {
        coinbase_value = coinbase_value
            .checked_add(output.get_value())
            .ok_or(AmountOverflowError)?;
        let outpoint = OutPoint::Coinbase {
            txid: coinbase_txid,
            vout: vout as u32,
        };
        let pointed_output = PointedOutput {
            outpoint,
            output: output.clone(),
        };
        accumulator_diff.insert((&pointed_output).into());
    }
    for (insert, utxo_hash) in accumulator_diff_txs {
        if insert {
            accumulator_diff.insert(utxo_hash);
        } else {
            accumulator_diff.remove(utxo_hash);
        }
    }
    // The game's cashouts are paid by the last coinbase outputs, exactly; the rest may claim the fees.
    let mut payouts_value = bitcoin::Amount::ZERO;
    for payout in payouts {
        payouts_value = payouts_value
            .checked_add(payout.get_value())
            .ok_or(AmountOverflowError)?;
    }
    if !body.coinbase.outputs.as_slice().ends_with(payouts) {
        return Err(Error::GamePayoutsMismatch);
    }
    let claimable = total_fees
        .checked_add(payouts_value)
        .ok_or(AmountOverflowError)?;
    if coinbase_value > claimable {
        return Err(Error::NotEnoughFees);
    }
    let spent_utxos = filled_transactions
        .iter()
        .flat_map(|t| t.spent_utxos.iter());
    for (authorization, spent_utxo) in
        body.authorizations.iter().zip(spent_utxos)
    {
        if authorization.get_address() != spent_utxo.address {
            return Err(Error::WrongPubKeyForAddress);
        }
    }
    if authorization::verify_authorizations(batch_verification_ctxt, body)
        .is_err()
    {
        return Err(Error::Authorization);
    }
    // Check root consistency without committing to DB
    let () = accumulator.apply_diff(accumulator_diff.clone())?;
    let roots: Vec<UtreexoNodeHash> = accumulator.get_roots();
    if roots != header.roots {
        return Err(Error::UtreexoRootsMismatch);
    }

    Ok(PrevalidatedBlock {
        filled_transactions,
        computed_merkle_root,
        total_fees,
        coinbase_value,
        next_height,
        accumulator_diff,
    })
}

/// Connect a block using the provided prevalidated data.
pub fn connect_prevalidated(
    state: &State,
    rwtxn: &mut RwTxn,
    header: &Header,
    body: &Body,
    pre: PrevalidatedBlock,
) -> Result<crate::types::MerkleRoot, Error> {
    let tip_hash = state.try_get_tip(rwtxn)?;
    if tip_hash != header.prev_side_hash {
        let err = error::InvalidHeader::PrevSideHash {
            expected: tip_hash,
            received: header.prev_side_hash,
        };
        return Err(Error::InvalidHeader(err));
    }
    if pre.computed_merkle_root != header.merkle_root {
        let err = Error::InvalidBody {
            expected: pre.computed_merkle_root,
            computed: header.merkle_root,
        };
        return Err(err);
    }
    let coinbase_txid = header.compute_coinbase_txid();

    // Apply UTXO set changes
    for (vout, output) in body.coinbase.outputs.iter().enumerate() {
        let outpoint = OutPoint::Coinbase {
            txid: coinbase_txid,
            vout: vout as u32,
        };
        state
            .utxos
            .put(rwtxn, &OutPointKey::from(&outpoint), output)
            .map_err(DbError::from)?;
    }

    for filled in &pre.filled_transactions {
        let txid = filled.transaction.txid();
        for (vin, (outpoint, _)) in filled.transaction.inputs.iter().enumerate()
        {
            let spent_output = state
                .utxos
                .try_get(rwtxn, &OutPointKey::from(outpoint))
                .map_err(DbError::from)?
                .ok_or(error::NoUtxo {
                    outpoint: *outpoint,
                })?;
            state
                .utxos
                .delete(rwtxn, &OutPointKey::from(outpoint))
                .map_err(DbError::from)?;
            let spent_output = SpentOutput {
                output: spent_output,
                inpoint: InPoint::Regular {
                    txid,
                    vin: vin as u32,
                },
            };
            state
                .stxos
                .put(rwtxn, &OutPointKey::from(outpoint), &spent_output)
                .map_err(DbError::from)?;
        }
        for (vout, output) in filled.transaction.outputs.iter().enumerate() {
            // Sidepot: a game deposit is not a coin, so the UTXO set never holds it. The outputs after it keep
            // their index.
            if output.content.is_game_deposit() {
                continue;
            }
            let outpoint = OutPoint::Regular {
                txid,
                vout: vout as u32,
            };
            state
                .utxos
                .put(rwtxn, &OutPointKey::from(&outpoint), output)
                .map_err(DbError::from)?;
        }
    }

    // Update tip/height
    let block_hash = header.hash();
    state
        .tip
        .put(rwtxn, &(), &block_hash)
        .map_err(DbError::from)?;
    state
        .height
        .put(rwtxn, &(), &pre.next_height)
        .map_err(DbError::from)?;

    // Apply accumulator diff
    let mut accumulator = state
        .utreexo_accumulator
        .try_get(rwtxn, &())
        .map_err(DbError::from)?
        .unwrap_or_default();
    let () = accumulator.apply_diff(pre.accumulator_diff)?;
    state
        .utreexo_accumulator
        .put(rwtxn, &(), &accumulator)
        .map_err(DbError::from)?;

    Ok(pre.computed_merkle_root)
}

pub fn validate(
    batch_verification_context: &BatchVerificationContext,
    state: &State,
    rotxn: &RoTxn,
    header: &Header,
    body: &Body,
) -> Result<(bitcoin::Amount, MerkleRoot), Error> {
    let tip_hash = state.try_get_tip(rotxn)?;
    if header.prev_side_hash != tip_hash {
        let err = error::InvalidHeader::PrevSideHash {
            expected: tip_hash,
            received: header.prev_side_hash,
        };
        return Err(Error::InvalidHeader(err));
    };
    // Sidepot: this path knows nothing of the game, so it takes no game deposits.
    let () = check_game_deposits(body, false)?;
    let height = state.try_get_height(rotxn)?.map_or(0, |height| height + 1);
    if body.authorizations.len() > State::body_sigops_limit(height) {
        return Err(Error::TooManySigops);
    }
    let body_size =
        borsh::object_length(&body).map_err(Error::BorshSerialize)?;
    if body_size > State::body_size_limit(height) {
        return Err(Error::BodyTooLarge);
    }
    let mut accumulator = state
        .utreexo_accumulator
        .try_get(rotxn, &())
        .map_err(DbError::from)?
        .unwrap_or_default();
    let filled_transactions: Vec<_> = body
        .transactions
        .iter()
        .map(|t| state.fill_transaction(rotxn, t))
        .collect::<Result<_, _>>()?;
    let merkle_root = Body::compute_merkle_root(
        &body.coinbase,
        filled_transactions.as_slice(),
    )?;
    if merkle_root != header.merkle_root {
        let err = Error::InvalidBody {
            expected: header.merkle_root,
            computed: merkle_root,
        };
        return Err(err);
    }
    let coinbase_txid = header.compute_coinbase_txid();
    let mut accumulator_diff = AccumulatorDiff::default();
    let mut coinbase_value = bitcoin::Amount::ZERO;
    for (vout, output) in body.coinbase.outputs.iter().enumerate() {
        coinbase_value = coinbase_value
            .checked_add(output.get_value())
            .ok_or(AmountOverflowError)?;
        let outpoint = OutPoint::Coinbase {
            txid: coinbase_txid,
            vout: vout as u32,
        };
        let pointed_output = PointedOutput {
            outpoint,
            output: output.clone(),
        };
        accumulator_diff.insert((&pointed_output).into());
    }
    let mut total_fees = bitcoin::Amount::ZERO;
    // Gather all input keys to check double-spends via sort-and-scan
    let total_inputs = body.inputs_len();
    let mut all_input_keys = Vec::with_capacity(total_inputs);
    for filled_transaction in &filled_transactions {
        let txid = filled_transaction.transaction.txid();
        // hashes of spent utxos, used to verify the utreexo proof
        let mut spent_utxo_hashes = Vec::<UtreexoNodeHash>::with_capacity(
            filled_transaction.transaction.inputs.len(),
        );
        for (outpoint, utxo_hash) in &filled_transaction.transaction.inputs {
            all_input_keys.push(OutPointKey::from(outpoint));
            spent_utxo_hashes.push(utxo_hash.into());
            accumulator_diff.remove(utxo_hash.into());
        }
        for (vout, output) in
            filled_transaction.transaction.outputs.iter().enumerate()
        {
            let outpoint = OutPoint::Regular {
                txid,
                vout: vout as u32,
            };
            let pointed_output = PointedOutput {
                outpoint,
                output: output.clone(),
            };
            accumulator_diff.insert((&pointed_output).into());
        }
        total_fees = total_fees
            .checked_add(state.validate_filled_transaction(filled_transaction)?)
            .ok_or(AmountOverflowError)?;
        // verify utreexo proof
        if !accumulator
            .verify(&filled_transaction.transaction.proof, &spent_utxo_hashes)?
        {
            return Err(Error::UtreexoProofFailed { txid });
        }
    }
    // Sort and check for duplicate outpoints (double-spend detection)
    {
        use rayon::prelude::ParallelSliceMut;
        all_input_keys.par_sort_unstable();
        if all_input_keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::UtxoDoubleSpent);
        }
    }
    if coinbase_value > total_fees {
        return Err(Error::NotEnoughFees);
    }
    let spent_utxos = filled_transactions
        .iter()
        .flat_map(|t| t.spent_utxos.iter());
    for (authorization, spent_utxo) in
        body.authorizations.iter().zip(spent_utxos)
    {
        if authorization.get_address() != spent_utxo.address {
            return Err(Error::WrongPubKeyForAddress);
        }
    }
    if authorization::verify_authorizations(batch_verification_context, body)
        .is_err()
    {
        return Err(Error::Authorization);
    }
    // Check root consistency without committing to DB
    let () = accumulator.apply_diff(accumulator_diff)?;
    let roots: Vec<UtreexoNodeHash> = accumulator.get_roots();
    if roots != header.roots {
        return Err(Error::UtreexoRootsMismatch);
    }
    Ok((total_fees, merkle_root))
}

pub fn connect(
    state: &State,
    rwtxn: &mut RwTxn,
    header: &Header,
    body: &Body,
) -> Result<MerkleRoot, Error> {
    let tip_hash = state.try_get_tip(rwtxn)?;
    if tip_hash != header.prev_side_hash {
        let err = error::InvalidHeader::PrevSideHash {
            expected: tip_hash,
            received: header.prev_side_hash,
        };
        return Err(Error::InvalidHeader(err));
    }
    // Sidepot: this path knows nothing of the game, so it takes no game deposits.
    let () = check_game_deposits(body, false)?;
    let coinbase_txid = header.compute_coinbase_txid();
    let mut accumulator = state
        .utreexo_accumulator
        .try_get(rwtxn, &())?
        .unwrap_or_default();
    let mut accumulator_diff = AccumulatorDiff::default();
    for (vout, output) in body.coinbase.outputs.iter().enumerate() {
        let outpoint = OutPoint::Coinbase {
            txid: coinbase_txid,
            vout: vout as u32,
        };
        let pointed_output = PointedOutput {
            outpoint,
            output: output.clone(),
        };
        accumulator_diff.insert((&pointed_output).into());
        let key = OutPointKey::from(&outpoint);
        state.utxos.put(rwtxn, &key, output)?;
    }
    let mut filled_txs: Vec<FilledTransaction> =
        Vec::with_capacity(body.transactions.len());
    for transaction in &body.transactions {
        let mut spent_utxos = Vec::with_capacity(transaction.inputs.len());
        let txid = transaction.txid();
        for (vin, (outpoint, utxo_hash)) in
            transaction.inputs.iter().enumerate()
        {
            let key = OutPointKey::from(outpoint);
            let spent_output =
                state.utxos.try_get(rwtxn, &key)?.ok_or(error::NoUtxo {
                    outpoint: *outpoint,
                })?;

            accumulator_diff.remove(utxo_hash.into());
            let key = OutPointKey::from(outpoint);
            state.utxos.delete(rwtxn, &key)?;
            let spent_output = SpentOutput {
                output: spent_output,
                inpoint: InPoint::Regular {
                    txid,
                    vin: vin as u32,
                },
            };
            state
                .stxos
                .put(rwtxn, &OutPointKey::from(outpoint), &spent_output)
                .map_err(DbError::from)?;
            spent_utxos.push(spent_output.output);
        }
        for (vout, output) in transaction.outputs.iter().enumerate() {
            let outpoint = OutPoint::Regular {
                txid,
                vout: vout as u32,
            };
            let pointed_output = PointedOutput {
                outpoint,
                output: output.clone(),
            };
            accumulator_diff.insert((&pointed_output).into());
            let key = OutPointKey::from(&outpoint);
            state.utxos.put(rwtxn, &key, output)?;
        }
        let filled_tx = FilledTransaction {
            spent_utxos,
            transaction: transaction.clone(),
        };
        filled_txs.push(filled_tx);
    }
    let merkle_root =
        Body::compute_merkle_root(&body.coinbase, filled_txs.as_slice())?;
    if merkle_root != header.merkle_root {
        let err = Error::InvalidBody {
            expected: header.merkle_root,
            computed: merkle_root,
        };
        return Err(err);
    }
    let block_hash = header.hash();
    let height = state.try_get_height(rwtxn)?.map_or(0, |height| height + 1);
    state.tip.put(rwtxn, &(), &block_hash)?;
    state.height.put(rwtxn, &(), &height)?;
    let () = accumulator.apply_diff(accumulator_diff)?;
    state.utreexo_accumulator.put(rwtxn, &(), &accumulator)?;
    Ok(merkle_root)
}

pub fn disconnect_tip(
    state: &State,
    rwtxn: &mut RwTxn,
    header: &Header,
    body: &Body,
) -> Result<(), Error> {
    let tip_hash = state
        .tip
        .try_get(rwtxn, &())
        .map_err(DbError::from)?
        .ok_or(Error::NoTip)?;
    let block_hash = header.hash();
    if tip_hash != block_hash {
        let err = error::InvalidHeader::BlockHash {
            expected: tip_hash,
            computed: block_hash,
        };
        return Err(Error::InvalidHeader(err));
    }
    let mut accumulator = state
        .utreexo_accumulator
        .try_get(rwtxn, &())
        .map_err(DbError::from)?
        .unwrap_or_default();
    tracing::debug!("Got acc");
    let mut accumulator_diff = AccumulatorDiff::default();
    // revert txs, last-to-first
    body.transactions.iter().rev().try_for_each(|tx| {
        let txid = tx.txid();
        // delete UTXOs, last-to-first
        tx.outputs.iter().enumerate().rev().try_for_each(
            |(vout, output)| {
                // Sidepot: a game deposit made no coin, so there is none to remove. Its credit is undone with
                // the block's game snapshot.
                if output.content.is_game_deposit() {
                    return Ok(());
                }
                let outpoint = OutPoint::Regular {
                    txid,
                    vout: vout as u32,
                };
                let pointed_output = PointedOutput {
                    outpoint,
                    output: output.clone(),
                };
                accumulator_diff.remove((&pointed_output).into());
                let key = OutPointKey::from(&outpoint);
                if state.utxos.delete(rwtxn, &key)? {
                    Ok::<_, Error>(())
                } else {
                    Err(error::NoUtxo { outpoint }.into())
                }
            },
        )?;
        // unspend STXOs, last-to-first
        tx.inputs
            .iter()
            .rev()
            .try_for_each(|(outpoint, utxo_hash)| {
                let key = OutPointKey::from(outpoint);
                if let Some(spent_output) = state.stxos.try_get(rwtxn, &key)? {
                    accumulator_diff.insert(utxo_hash.into());
                    state.stxos.delete(rwtxn, &key)?;
                    state.utxos.put(rwtxn, &key, &spent_output.output)?;
                    Ok(())
                } else {
                    Err(Error::NoStxo {
                        outpoint: *outpoint,
                    })
                }
            })
    })?;
    // delete coinbase UTXOs, last-to-first
    {
        let coinbase_txid = header.compute_coinbase_txid();
        body.coinbase
            .outputs
            .iter()
            .enumerate()
            .rev()
            .try_for_each(|(vout, output)| {
                let outpoint = OutPoint::Coinbase {
                    txid: coinbase_txid,
                    vout: vout as u32,
                };
                let pointed_output = PointedOutput {
                    outpoint,
                    output: output.clone(),
                };
                accumulator_diff.remove((&pointed_output).into());
                let key = OutPointKey::from(&outpoint);
                if state.utxos.delete(rwtxn, &key)? {
                    Ok::<_, Error>(())
                } else {
                    Err(error::NoUtxo { outpoint }.into())
                }
            })?;
    }
    let height = state
        .try_get_height(rwtxn)?
        .expect("Height should not be None");
    match (header.prev_side_hash, height) {
        (None, 0) => {
            state.tip.delete(rwtxn, &()).map_err(DbError::from)?;
            state.height.delete(rwtxn, &()).map_err(DbError::from)?;
        }
        (None, _) | (_, 0) => return Err(Error::NoTip),
        (Some(prev_side_hash), height) => {
            state
                .tip
                .put(rwtxn, &(), &prev_side_hash)
                .map_err(DbError::from)?;
            state
                .height
                .put(rwtxn, &(), &(height - 1))
                .map_err(DbError::from)?;
        }
    }
    let () = accumulator.apply_diff(accumulator_diff)?;
    state
        .utreexo_accumulator
        .put(rwtxn, &(), &accumulator)
        .map_err(DbError::from)?;
    Ok(())
}

#[cfg(test)]
mod test {
    use thunder_types::Coinbase;

    use crate::state::test::{fresh_state, value_output};

    #[test]
    fn validation_rejects_outpoint_utxo_hash_mismatch() -> anyhow::Result<()> {
        use bitcoin::hashes::Hash as _;

        use crate::types::{
            Accumulator, AccumulatorDiff, Body, Header, OutPoint, OutPointKey,
            PointedOutput, Transaction, UtreexoNodeHash,
            authorization::{
                self, BatchVerificationContext, SigningKey, get_address,
            },
            hash,
        };

        let (_temp_dir, env, state) =
            fresh_state("validation_rejects_outpoint_utxo_hash_mismatch")?;
        let mut rng = rand::rng();
        let batch_verification_ctxt = BatchVerificationContext::new(&mut rng);

        // Attacker key (owns A). Victim key (owns B).
        let attacker = SigningKey::new(&mut rng);
        let attacker_addr = get_address((&attacker).into());
        let victim = SigningKey::new(&mut rng);
        let victim_addr = get_address(victim.into());

        // UTXO A (attacker, 10_000) and victim UTXO B (20_000).
        let outpoint_a = OutPoint::Deposit(bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array([0xAA; 32]),
            vout: 0,
        });
        let output_a = value_output(attacker_addr, 10_000);
        let outpoint_b = OutPoint::Deposit(bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array([0xBB; 32]),
            vout: 0,
        });
        let output_b = value_output(victim_addr, 20_000);

        // Leaf hashes for A and B (the Utreexo commitments).
        let pointed_a = PointedOutput {
            outpoint: outpoint_a,
            output: output_a.clone(),
        };
        let pointed_b = PointedOutput {
            outpoint: outpoint_b,
            output: output_b.clone(),
        };
        let leaf_a: UtreexoNodeHash = (&pointed_a).into();
        let leaf_b: UtreexoNodeHash = (&pointed_b).into();
        let hash_b: crate::types::Hash = hash(&pointed_b); // input's utxo_hash

        // Helper: build a fresh accumulator seeded with leaves A and B
        // (Accumulator is not Clone, so re-seed when a fresh copy is needed).
        let seeded_accumulator = || -> anyhow::Result<_> {
            let mut acc = Accumulator::default();
            let mut diff = AccumulatorDiff::default();
            diff.insert(leaf_a);
            diff.insert(leaf_b);
            acc.apply_diff(diff)?;
            Ok(acc)
        };

        // Seed UTXO DB with A and B; seed the accumulator with both leaves.
        let pre_accumulator = seeded_accumulator()?;
        {
            let mut rwtxn = env.write_txn()?;
            state.utxos.put(
                &mut rwtxn,
                &OutPointKey::from(&outpoint_a),
                &output_a,
            )?;
            state.utxos.put(
                &mut rwtxn,
                &OutPointKey::from(&outpoint_b),
                &output_b,
            )?;
            state
                .utreexo_accumulator
                .put(&mut rwtxn, &(), &pre_accumulator)?;
            // tip stays unset (None) so validate's prev_side_hash check
            // expects header.prev_side_hash == None.
            rwtxn.commit()?;
        }

        // Build the malicious tx: input = (outpoint_A, hash_B) + proof for B;
        // output C = 9_000 to the attacker.
        let proof_for_b = pre_accumulator.prove(&[leaf_b])?;
        let output_c = value_output(attacker_addr, 9_000);
        let tx = Transaction {
            inputs: vec![(outpoint_a, hash_b)].into(),
            proof: proof_for_b,
            outputs: vec![output_c.clone()].into(),
        };
        // Sign with A's key (the spender of outpoint A authorizes the tx).
        let authorized = authorization::authorize(
            &mut rng,
            &[(attacker_addr, &attacker)],
            tx,
        )?;

        // Assemble body.
        let body = Body::new(vec![authorized], Coinbase::default());

        // Compute the header the validator expects:
        //   merkle_root from the filled tx, roots = post-block accumulator
        //   (B's leaf removed, C's leaf inserted) -- exactly the diff validate
        //   builds from the SUPPLIED utxo_hash.
        let filled = {
            let rotxn = env.read_txn()?;
            state.fill_transaction(&rotxn, &body.transactions[0])?
        };

        // tx validation REJECTS the outpoint/utxo_hash mismatch.
        anyhow::ensure!(state.validate_filled_transaction(&filled).is_err());
        let merkle_root = Body::compute_merkle_root(&body.coinbase, &[filled])?;
        let mut post_accumulator = seeded_accumulator()?;
        {
            let mut diff = AccumulatorDiff::default();
            // validate removes the SUPPLIED utxo_hash (B), inserts output C.
            diff.remove(leaf_b);
            let txid = body.transactions[0].txid();
            let pointed_c = PointedOutput {
                outpoint: OutPoint::Regular { txid, vout: 0 },
                output: output_c.clone(),
            };
            diff.insert((&pointed_c).into());
            post_accumulator.apply_diff(diff)?;
        }
        let header = Header {
            producer: [0; 32],
            game_hash: [0; 32],
            merkle_root,
            prev_side_hash: None,
            prev_main_hash: bitcoin::BlockHash::from_byte_array([0u8; 32]),
            roots: post_accumulator.get_roots(),
        };

        // block validation REJECTS the outpoint/utxo_hash mismatch.
        {
            let rotxn = env.read_txn()?;
            anyhow::ensure!(
                state
                    .validate_block(
                        &rotxn,
                        &batch_verification_ctxt,
                        &header,
                        &body
                    )
                    .is_err(),
                "BUG: real validate_block accepts an input whose outpoint (A) \
                and utxo_hash (B) refer to different UTXOs",
            );
        }
        Ok(())
    }
}

/// Sidepot: game deposit outputs (an ordinary coin moved into a game account by a sidechain transaction).
#[cfg(test)]
mod game_deposit_test {
    use bitcoin::hashes::Hash as _;

    use crate::{
        state::{
            Error, State,
            test::{fresh_state, value_output},
        },
        types::{
            Accumulator, AccumulatorDiff, Address, Body, Coinbase,
            FilledTransaction, Header, OutPoint, OutPointKey, Output,
            OutputContent, PointedOutput, Transaction, UtreexoNodeHash,
            authorization::{
                self, BatchVerificationContext, SigningKey, get_address,
            },
            hash,
        },
    };

    /// The secp256k1 generator's x coordinate: a valid game account key.
    const ACCOUNT: [u8; 32] = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95,
        0xce, 0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9,
        0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17, 0x98,
    ];
    /// Twice the generator's x coordinate: another valid key.
    const ACCOUNT_2: [u8; 32] = [
        0xc6, 0x04, 0x7f, 0x94, 0x41, 0xed, 0x7d, 0x6d, 0x30, 0x45, 0x40, 0x6e,
        0x95, 0xc0, 0x7c, 0xd8, 0x5c, 0x77, 0x8e, 0x4b, 0x8c, 0xef, 0x3c, 0xa7,
        0xab, 0xac, 0x09, 0xb9, 0x5c, 0x70, 0x9e, 0xe5,
    ];
    const FUNDS: u64 = 10_000;

    fn game_deposit(sats: u64, account: [u8; 32]) -> Output {
        Output {
            address: Address::ALL_ZEROS,
            content: OutputContent::GameDeposit {
                value: bitcoin::Amount::from_sat(sats),
                account,
            },
        }
    }

    /// A state holding one coin of `FUNDS` sats, and what spends it.
    struct Fixture {
        _temp_dir: temp_dir::TempDir,
        env: sneed::Env,
        state: State,
        ctxt: BatchVerificationContext,
        key: SigningKey,
        address: Address,
        outpoint: OutPoint,
        coin: Output,
    }

    impl Fixture {
        fn new(name: &str) -> anyhow::Result<Self> {
            let (temp_dir, env, state) = fresh_state(name)?;
            let mut rng = rand::rng();
            let ctxt = BatchVerificationContext::new(&mut rng);
            let key = SigningKey::new(&mut rng);
            let address = get_address((&key).into());
            let outpoint = OutPoint::Deposit(bitcoin::OutPoint {
                txid: bitcoin::Txid::from_byte_array([0xAA; 32]),
                vout: 0,
            });
            let coin = value_output(address, FUNDS);
            let mut rwtxn = env.write_txn()?;
            state.utxos.put(
                &mut rwtxn,
                &OutPointKey::from(&outpoint),
                &coin,
            )?;
            rwtxn.commit()?;
            let fixture = Self {
                _temp_dir: temp_dir,
                env,
                state,
                ctxt,
                key,
                address,
                outpoint,
                coin,
            };
            fixture.reset_accumulator()?;
            Ok(fixture)
        }

        /// Stores the accumulator of the state before any block: the one coin. The node does the same after a
        /// disconnect, from its archive, since the accumulator cannot undo.
        fn reset_accumulator(&self) -> anyhow::Result<()> {
            let mut accumulator = Accumulator::default();
            let mut diff = AccumulatorDiff::default();
            diff.insert(UtreexoNodeHash::from(&PointedOutput {
                outpoint: self.outpoint,
                output: self.coin.clone(),
            }));
            accumulator.apply_diff(diff)?;
            let mut rwtxn = self.env.write_txn()?;
            self.state.utreexo_accumulator.put(
                &mut rwtxn,
                &(),
                &accumulator,
            )?;
            rwtxn.commit()?;
            Ok(())
        }

        fn filled(&self, outputs: Vec<Output>) -> FilledTransaction {
            let pointed = PointedOutput {
                outpoint: self.outpoint,
                output: self.coin.clone(),
            };
            FilledTransaction {
                transaction: Transaction {
                    inputs: vec![(self.outpoint, hash(&pointed))].into(),
                    outputs: outputs.into(),
                    ..Default::default()
                },
                spent_utxos: vec![self.coin.clone()],
            }
        }

        /// A block, built the way the node builds one (`Body::modify_memforest` gives the header's roots),
        /// whose one transaction spends the coin into `outputs`.
        fn block(
            &self,
            outputs: Vec<Output>,
            coinbase: Vec<Output>,
        ) -> anyhow::Result<(Header, Body)> {
            let rotxn = self.env.read_txn()?;
            let mut accumulator = self.state.get_accumulator(&rotxn)?;
            let mut filled = self.filled(outputs);
            filled.transaction.proof =
                accumulator.prove(&[UtreexoNodeHash::from(
                    &PointedOutput {
                        outpoint: self.outpoint,
                        output: self.coin.clone(),
                    },
                )])?;
            let authorized = authorization::authorize(
                rand::rng(),
                &[(self.address, &self.key)],
                filled.transaction.clone(),
            )?;
            let coinbase = Coinbase {
                memo: Vec::new(),
                outputs: coinbase.into(),
            };
            let merkle_root =
                Body::compute_merkle_root(&coinbase, &[filled.clone()])?;
            let prev_main_hash = bitcoin::BlockHash::all_zeros();
            let coinbase_txid =
                Coinbase::compute_txid(&merkle_root, &prev_main_hash, None);
            Body::modify_memforest(
                coinbase_txid,
                coinbase.outputs.as_slice(),
                &[filled],
                &mut accumulator.0,
            )?;
            let roots = accumulator
                .0
                .get_roots()
                .iter()
                .map(|root| root.get_data())
                .collect();
            let header = Header {
                producer: [0; 32],
                game_hash: [0; 32],
                merkle_root,
                prev_side_hash: None,
                prev_main_hash,
                roots,
            };
            Ok((header, Body::new(vec![authorized], coinbase)))
        }

        fn prevalidate(
            &self,
            block: &(Header, Body),
            game_deposits_active: bool,
        ) -> Result<crate::state::PrevalidatedBlock, Error> {
            let rotxn = self.env.read_txn().unwrap();
            self.state.prevalidate_block_with_payouts(
                &rotxn,
                &self.ctxt,
                &block.0,
                &block.1,
                &[],
                game_deposits_active,
            )
        }

        fn coins(&self) -> anyhow::Result<Vec<(OutPoint, Output)>> {
            let rotxn = self.env.read_txn()?;
            let mut coins: Vec<_> =
                self.state.get_utxos(&rotxn)?.into_iter().collect();
            coins.sort_by_key(|(outpoint, _)| OutPointKey::from(outpoint));
            Ok(coins)
        }
    }

    /// Binary (bincode) encoding, as on the wire and in the databases. The first two values were printed by
    /// the code as it was before the game deposit variant existed (branch `sidepot` at 7674a3e).
    #[test]
    fn binary_encodings() -> anyhow::Result<()> {
        let value = value_output(Address([0x11; 20]), 123_456_789);
        anyhow::ensure!(
            const_hex::encode(bincode::serialize(&value)?)
                == "11111111111111111111111111111111111111110000000015cd5b0700000000"
        );
        let withdrawal = Output {
            address: Address::ALL_ZEROS,
            content: OutputContent::Withdrawal {
                value: bitcoin::Amount::from_sat(1000),
                main_fee: bitcoin::Amount::from_sat(300),
                main_address: "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2".parse()?,
            },
        };
        anyhow::ensure!(
            const_hex::encode(bincode::serialize(&withdrawal)?)
                == "000000000000000000000000000000000000000001000000e803000000000000\
                    2c010000000000002200000000000000314276424d5345597374576574715446\
                    6e354175346d3447466737784a614e564e32"
        );
        // A game deposit: address, variant 2 (u32), value (u64), the 32 account bytes.
        let deposit = game_deposit(5000, ACCOUNT);
        let bytes = bincode::serialize(&deposit)?;
        anyhow::ensure!(
            const_hex::encode(&bytes)
                == format!(
                    "{}020000008813000000000000{}",
                    "00".repeat(20),
                    const_hex::encode(ACCOUNT)
                )
        );
        anyhow::ensure!(bincode::deserialize::<Output>(&bytes)? == deposit);
        Ok(())
    }

    #[test]
    fn refused_unless_active() -> anyhow::Result<()> {
        let fixture = Fixture::new("game-deposit-refused-unless-active")?;
        let block = fixture.block(
            vec![
                game_deposit(6000, ACCOUNT),
                value_output(fixture.address, 3500),
            ],
            Vec::new(),
        )?;
        anyhow::ensure!(matches!(
            fixture.prevalidate(&block, false),
            Err(Error::GameDepositsNotActive)
        ));
        // The paths that know nothing of the game never take one.
        let rotxn = fixture.env.read_txn()?;
        anyhow::ensure!(matches!(
            fixture.state.prevalidate_block(
                &rotxn,
                &fixture.ctxt,
                &block.0,
                &block.1
            ),
            Err(Error::GameDepositsNotActive)
        ));
        anyhow::ensure!(matches!(
            fixture.state.validate_block(
                &rotxn,
                &fixture.ctxt,
                &block.0,
                &block.1
            ),
            Err(Error::GameDepositsNotActive)
        ));
        drop(rotxn);
        let mut rwtxn = fixture.env.write_txn()?;
        anyhow::ensure!(matches!(
            fixture.state.connect_block(&mut rwtxn, &block.0, &block.1),
            Err(Error::GameDepositsNotActive)
        ));
        drop(rwtxn);
        // Active, the same block is valid.
        anyhow::ensure!(fixture.prevalidate(&block, true).is_ok());
        Ok(())
    }

    #[test]
    fn connects_without_a_coin_and_disconnects() -> anyhow::Result<()> {
        let fixture = Fixture::new("game-deposit-connects")?;
        let before = fixture.coins()?;
        anyhow::ensure!(
            before == vec![(fixture.outpoint, fixture.coin.clone())]
        );
        // 10,000 in: 6,000 to the game, 3,500 change, 500 fee, which the coinbase claims.
        let block = fixture.block(
            vec![
                game_deposit(6000, ACCOUNT),
                value_output(fixture.address, 3500),
            ],
            vec![value_output(fixture.address, 500)],
        )?;
        let (header, body) = &block;
        let sat = bitcoin::Amount::from_sat;
        anyhow::ensure!(body.game_deposits() == vec![(ACCOUNT, sat(6000))]);
        let prevalidated = fixture.prevalidate(&block, true)?;
        anyhow::ensure!(prevalidated.total_fees == sat(500));
        let mut rwtxn = fixture.env.write_txn()?;
        fixture.state.connect_prevalidated_block(
            &mut rwtxn,
            header,
            body,
            prevalidated,
        )?;
        rwtxn.commit()?;

        let txid = body.transactions[0].txid();
        let coins = fixture.coins()?;
        let mut expected = vec![
            (
                OutPoint::Coinbase {
                    txid: header.compute_coinbase_txid(),
                    vout: 0,
                },
                value_output(fixture.address, 500),
            ),
            // the change keeps its index, 1; nothing exists at index 0
            (
                OutPoint::Regular { txid, vout: 1 },
                value_output(fixture.address, 3500),
            ),
        ];
        expected.sort_by_key(|(outpoint, _)| OutPointKey::from(outpoint));
        anyhow::ensure!(coins == expected, "{coins:?}");
        // The coins lost exactly the deposit: 10,000 before, 4,000 after.
        let total: u64 = coins
            .iter()
            .map(|(_, output)| {
                crate::types::GetValue::get_value(output).to_sat()
            })
            .sum();
        anyhow::ensure!(total == FUNDS - 6000);
        {
            let rotxn = fixture.env.read_txn()?;
            // the accumulator the state holds is the one the header commits to
            anyhow::ensure!(
                fixture.state.get_accumulator(&rotxn)?.get_roots()
                    == header.roots
            );
            anyhow::ensure!(
                fixture
                    .state
                    .stxos
                    .try_get(&rotxn, &OutPointKey::from(&fixture.outpoint))?
                    .is_some()
            );
        }

        // Disconnecting puts the coins back exactly; there is no coin to remove for the deposit.
        let mut rwtxn = fixture.env.write_txn()?;
        fixture.state.disconnect_tip(&mut rwtxn, header, body)?;
        rwtxn.commit()?;
        anyhow::ensure!(fixture.coins()? == before);
        let rotxn = fixture.env.read_txn()?;
        anyhow::ensure!(fixture.state.try_get_tip(&rotxn)?.is_none());
        anyhow::ensure!(
            fixture
                .state
                .stxos
                .try_get(&rotxn, &OutPointKey::from(&fixture.outpoint))?
                .is_none()
        );
        drop(rotxn);
        // And the same block connects again.
        fixture.reset_accumulator()?;
        let prevalidated = fixture.prevalidate(&block, true)?;
        let mut rwtxn = fixture.env.write_txn()?;
        fixture.state.connect_prevalidated_block(
            &mut rwtxn,
            header,
            body,
            prevalidated,
        )?;
        rwtxn.commit()?;
        anyhow::ensure!(fixture.coins()? == expected);
        Ok(())
    }

    /// The value of a game deposit is not a fee: the coinbase cannot claim it.
    #[test]
    fn value_is_conserved() -> anyhow::Result<()> {
        let fixture = Fixture::new("game-deposit-value-conserved")?;
        let outputs = || {
            vec![
                game_deposit(6000, ACCOUNT),
                value_output(fixture.address, 3500),
            ]
        };
        for claimed in [501, 6500] {
            let block = fixture.block(
                outputs(),
                vec![value_output(fixture.address, claimed)],
            )?;
            anyhow::ensure!(matches!(
                fixture.prevalidate(&block, true),
                Err(Error::NotEnoughFees)
            ));
        }
        // Inputs must cover every output, game deposits included.
        let overspend = fixture.filled(vec![
            game_deposit(6000, ACCOUNT),
            value_output(fixture.address, 4001),
        ]);
        anyhow::ensure!(matches!(
            fixture.state.validate_filled_transaction(&overspend),
            Err(Error::NotEnoughValueIn)
        ));
        let exact = fixture.filled(vec![
            game_deposit(6000, ACCOUNT),
            value_output(fixture.address, 4000),
        ]);
        anyhow::ensure!(
            fixture.state.validate_filled_transaction(&exact)?
                == bitcoin::Amount::ZERO
        );
        let all_in = fixture.filled(vec![game_deposit(FUNDS, ACCOUNT)]);
        anyhow::ensure!(
            fixture.state.validate_filled_transaction(&all_in)?
                == bitcoin::Amount::ZERO
        );
        Ok(())
    }

    #[test]
    fn malformed_outputs_are_invalid() -> anyhow::Result<()> {
        let fixture = Fixture::new("game-deposit-malformed")?;
        // zero value
        let block =
            fixture.block(vec![game_deposit(0, ACCOUNT)], Vec::new())?;
        anyhow::ensure!(matches!(
            fixture.prevalidate(&block, true),
            Err(Error::GameDepositZeroValue)
        ));
        // an address other than all zeros
        let mut addressed = game_deposit(1000, ACCOUNT);
        addressed.address = fixture.address;
        let block = fixture.block(vec![addressed], Vec::new())?;
        anyhow::ensure!(matches!(
            fixture.prevalidate(&block, true),
            Err(Error::GameDepositAddress)
        ));
        // an account that is not a key anyone could sign for
        for account in [[0xff; 32], [0; 32]] {
            let block =
                fixture.block(vec![game_deposit(1000, account)], Vec::new())?;
            anyhow::ensure!(matches!(
                fixture.prevalidate(&block, true),
                Err(Error::GameDepositAccount)
            ));
        }
        // a coinbase output, active or not
        let block = fixture.block(
            vec![value_output(fixture.address, 9000)],
            vec![game_deposit(1000, ACCOUNT)],
        )?;
        for active in [true, false] {
            anyhow::ensure!(matches!(
                fixture.prevalidate(&block, active),
                Err(Error::GameDepositInCoinbase)
            ));
        }
        Ok(())
    }

    #[test]
    fn several_deposits_among_ordinary_outputs() -> anyhow::Result<()> {
        let fixture = Fixture::new("game-deposit-several")?;
        let block = fixture.block(
            vec![
                value_output(fixture.address, 1000),
                game_deposit(2000, ACCOUNT),
                value_output(fixture.address, 3000),
                game_deposit(500, ACCOUNT_2),
                game_deposit(1, ACCOUNT),
            ],
            Vec::new(),
        )?;
        let (header, body) = &block;
        let sat = bitcoin::Amount::from_sat;
        anyhow::ensure!(
            body.game_deposits()
                == vec![
                    (ACCOUNT, sat(2000)),
                    (ACCOUNT_2, sat(500)),
                    (ACCOUNT, sat(1))
                ]
        );
        let prevalidated = fixture.prevalidate(&block, true)?;
        anyhow::ensure!(prevalidated.total_fees == sat(FUNDS - 6501));
        let mut rwtxn = fixture.env.write_txn()?;
        fixture.state.connect_prevalidated_block(
            &mut rwtxn,
            header,
            body,
            prevalidated,
        )?;
        rwtxn.commit()?;
        let txid = body.transactions[0].txid();
        let mut expected = vec![
            (
                OutPoint::Regular { txid, vout: 0 },
                value_output(fixture.address, 1000),
            ),
            (
                OutPoint::Regular { txid, vout: 2 },
                value_output(fixture.address, 3000),
            ),
        ];
        expected.sort_by_key(|(outpoint, _)| OutPointKey::from(outpoint));
        anyhow::ensure!(fixture.coins()? == expected);
        Ok(())
    }
}
