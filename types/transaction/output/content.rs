use serde::{Deserialize, Serialize};
use utoipa::{PartialSchema, ToSchema};

use crate::{schema, transaction::GetValue, util};

/// Default representation for Serde.
///
/// Note that [`SerdeRepr`] below gives this one to human-readable formats (it is what JSON shows: `value`, not
/// `value_sats`) and [`HumanReadableRepr`] to binary ones. Both are encodings in use, on the wire and in
/// databases, so neither may change; a new variant is appended to both.
#[derive(Deserialize, Serialize)]
enum DefaultRepr {
    Value(bitcoin::Amount),
    Withdrawal {
        value: bitcoin::Amount,
        main_fee: bitcoin::Amount,
        main_address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    },
    // Appended after the variants above: their encodings must not change.
    GameDeposit {
        value: bitcoin::Amount,
        #[serde(with = "account_serde")]
        account: [u8; 32],
    },
}

/// A game account key: exactly 64 hex digits in a human-readable format (JSON), its 32 bytes in a binary one.
/// It asks the format itself, so it gives the same result in either representation below.
mod account_serde {
    use serde::{
        Deserialize as _, Deserializer, Serialize as _, Serializer,
        de::Error as _,
    };

    pub fn serialize<S>(account: &[u8; 32], ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if ser.is_human_readable() {
            ser.serialize_str(&const_hex::encode(account))
        } else {
            account.serialize(ser)
        }
    }

    pub fn deserialize<'de, D>(de: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        if !de.is_human_readable() {
            return <[u8; 32]>::deserialize(de);
        }
        let hex = String::deserialize(de)?;
        let mut account = [0; 32];
        if hex.len() != 64
            || const_hex::decode_to_slice(&hex, &mut account).is_err()
        {
            return Err(D::Error::custom("a game account is 64 hex digits"));
        }
        Ok(account)
    }
}

/// Human-readable representation for Serde
#[derive(Deserialize, Serialize, ToSchema)]
#[schema(as = OutputContent, description = "")]
enum HumanReadableRepr {
    #[schema(value_type = u64)]
    Value(#[serde(with = "bitcoin::amount::serde::as_sat")] bitcoin::Amount),
    Withdrawal {
        #[serde(with = "bitcoin::amount::serde::as_sat")]
        #[serde(rename = "value_sats")]
        #[schema(value_type = u64)]
        value: bitcoin::Amount,
        #[serde(with = "bitcoin::amount::serde::as_sat")]
        #[serde(rename = "main_fee_sats")]
        #[schema(value_type = u64)]
        main_fee: bitcoin::Amount,
        #[schema(value_type = schema::BitcoinAddr)]
        main_address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    },
    GameDeposit {
        #[serde(with = "bitcoin::amount::serde::as_sat")]
        #[serde(rename = "value_sats")]
        #[schema(value_type = u64)]
        value: bitcoin::Amount,
        /// The game account key, 64 hex digits
        #[serde(with = "account_serde")]
        #[schema(value_type = String)]
        account: [u8; 32],
    },
}

type SerdeRepr = serde_with::IfIsHumanReadable<
    serde_with::FromInto<DefaultRepr>,
    serde_with::FromInto<HumanReadableRepr>,
>;

#[derive(borsh::BorshSerialize, Clone, Debug, Eq, PartialEq)]
pub enum Content {
    Value(
        #[borsh(serialize_with = "util::borsh::serialize::bitcoin_amount")]
        bitcoin::Amount,
    ),
    Withdrawal {
        #[borsh(serialize_with = "util::borsh::serialize::bitcoin_amount")]
        value: bitcoin::Amount,
        #[borsh(serialize_with = "util::borsh::serialize::bitcoin_amount")]
        main_fee: bitcoin::Amount,
        #[borsh(serialize_with = "util::borsh::serialize::bitcoin_address")]
        main_address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    },
    /// Sidepot: moves `value` into the game account `account` (its 32-byte
    /// key) when the block that holds the transaction is connected. Never a
    /// coin: it is not added to the UTXO set and cannot be spent. The
    /// output's address is not used and must be all zeros.
    ///
    /// Appended after the variants above (canonical tag 2): their encodings
    /// and hashes must not change.
    GameDeposit {
        #[borsh(serialize_with = "util::borsh::serialize::bitcoin_amount")]
        value: bitcoin::Amount,
        account: [u8; 32],
    },
}

impl Content {
    pub fn is_value(&self) -> bool {
        matches!(self, Self::Value(_))
    }
    pub fn is_withdrawal(&self) -> bool {
        matches!(self, Self::Withdrawal { .. })
    }
    pub fn is_game_deposit(&self) -> bool {
        matches!(self, Self::GameDeposit { .. })
    }
}

impl GetValue for Content {
    #[inline(always)]
    fn get_value(&self) -> bitcoin::Amount {
        match self {
            Self::Value(value) => *value,
            // a withdrawal removes both the payout and the mainchain fee
            // from the sidechain, since the enforcer pays both out of the
            // treasury
            Self::Withdrawal {
                value, main_fee, ..
            } => value.checked_add(*main_fee).unwrap_or(bitcoin::Amount::MAX),
            // the value leaves the coins for the game, so the transaction
            // must fund it like any other output
            Self::GameDeposit { value, .. } => *value,
        }
    }
}

impl From<Content> for DefaultRepr {
    fn from(content: Content) -> Self {
        match content {
            Content::Value(value) => Self::Value(value),
            Content::Withdrawal {
                value,
                main_fee,
                main_address,
            } => Self::Withdrawal {
                value,
                main_fee,
                main_address,
            },
            Content::GameDeposit { value, account } => {
                Self::GameDeposit { value, account }
            }
        }
    }
}

impl From<Content> for HumanReadableRepr {
    fn from(content: Content) -> Self {
        match content {
            Content::Value(value) => Self::Value(value),
            Content::Withdrawal {
                value,
                main_fee,
                main_address,
            } => Self::Withdrawal {
                value,
                main_fee,
                main_address,
            },
            Content::GameDeposit { value, account } => {
                Self::GameDeposit { value, account }
            }
        }
    }
}

impl From<DefaultRepr> for Content {
    fn from(repr: DefaultRepr) -> Self {
        match repr {
            DefaultRepr::Value(value) => Self::Value(value),
            DefaultRepr::Withdrawal {
                value,
                main_fee,
                main_address,
            } => Self::Withdrawal {
                value,
                main_fee,
                main_address,
            },
            DefaultRepr::GameDeposit { value, account } => {
                Self::GameDeposit { value, account }
            }
        }
    }
}

impl From<HumanReadableRepr> for Content {
    fn from(repr: HumanReadableRepr) -> Self {
        match repr {
            HumanReadableRepr::Value(value) => Self::Value(value),
            HumanReadableRepr::Withdrawal {
                value,
                main_fee,
                main_address,
            } => Self::Withdrawal {
                value,
                main_fee,
                main_address,
            },
            HumanReadableRepr::GameDeposit { value, account } => {
                Self::GameDeposit { value, account }
            }
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <SerdeRepr as serde_with::DeserializeAs<'de, _>>::deserialize_as(
            deserializer,
        )
    }
}

impl Serialize for Content {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        <SerdeRepr as serde_with::SerializeAs<_>>::serialize_as(
            self, serializer,
        )
    }
}

impl PartialSchema for Content {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        <HumanReadableRepr as PartialSchema>::schema()
    }
}

impl ToSchema for Content {
    fn name() -> std::borrow::Cow<'static, str> {
        <HumanReadableRepr as ToSchema>::name()
    }
}

#[cfg(test)]
mod test {
    use super::Content;
    use crate::{
        address::Address,
        hashes::hash,
        transaction::{
            GetValue as _, OutPoint, Output, PointedOutput, Transaction,
        },
    };

    /// The secp256k1 generator's x coordinate: a valid game account key.
    const ACCOUNT: [u8; 32] = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95,
        0xce, 0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9,
        0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17, 0x98,
    ];
    const ACCOUNT_HEX: &str =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn value_output() -> Output {
        Output {
            address: Address([0x11; 20]),
            content: Content::Value(bitcoin::Amount::from_sat(123_456_789)),
        }
    }

    fn withdrawal_output() -> Output {
        Output {
            address: Address::ALL_ZEROS,
            content: Content::Withdrawal {
                value: bitcoin::Amount::from_sat(1000),
                main_fee: bitcoin::Amount::from_sat(300),
                main_address: "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"
                    .parse()
                    .unwrap(),
            },
        }
    }

    fn game_deposit_output() -> Output {
        Output {
            address: Address::ALL_ZEROS,
            content: Content::GameDeposit {
                value: bitcoin::Amount::from_sat(5000),
                account: ACCOUNT,
            },
        }
    }

    fn pointed(output: &Output) -> PointedOutput {
        PointedOutput {
            outpoint: OutPoint::Regular {
                txid: [7; 32].into(),
                vout: 1,
            },
            output: output.clone(),
        }
    }

    fn hex<T: borsh::BorshSerialize>(value: &T) -> String {
        const_hex::encode(borsh::to_vec(value).unwrap())
    }

    // The values pinned in the two tests below were printed by the code as it was before the game deposit
    // variant existed (branch `sidepot` at 7674a3e). They must never change: existing chains hash these.
    #[test]
    fn value_encoding_and_hashes_are_unchanged() {
        let output = value_output();
        assert_eq!(
            hex(&output),
            "11111111111111111111111111111111111111110015cd5b0700000000"
        );
        assert_eq!(
            const_hex::encode(hash(&output)),
            "23a9510d3cf1059d7394806f7cd21fdfceb49bf3884cfbd90fb05425cc420a46"
        );
        assert_eq!(
            const_hex::encode(hash(&pointed(&output))),
            "d9eaad81da5ab80b8de36fa9fb8b181ed5817cc8add8b04c4989f5a3a1d81824"
        );
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"address":"EnrGHeqCd5UQ2jTW2Mo32o6a2GG","content":{"Value":123456789}}"#
        );
    }

    #[test]
    fn withdrawal_encoding_and_hashes_are_unchanged() {
        let output = withdrawal_output();
        assert_eq!(
            hex(&output),
            "000000000000000000000000000000000000000001e8030000000000002c0100\
             00000000001900000076a91477bff20c60e522dfaa3350c39b030a5d004e839a\
             88ac"
        );
        assert_eq!(
            const_hex::encode(hash(&output)),
            "7985f936d4956a4ae2a68431e17d4123abbe852e5b81f84b35932e29d001e81b"
        );
        assert_eq!(
            const_hex::encode(hash(&pointed(&output))),
            "4e29f3dd1d08a44742127bebab28236a6ca1863278e40379b3c1c565df331b35"
        );
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"address":"11111111111111111111","content":{"Withdrawal":{"value":1000,"main_fee":300,"main_address":"1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"}}}"#
        );
    }

    #[test]
    fn transaction_id_is_unchanged() {
        let tx = Transaction {
            inputs: vec![(
                OutPoint::Regular {
                    txid: [1; 32].into(),
                    vout: 0,
                },
                [2; 32],
            )]
            .into(),
            outputs: vec![value_output(), withdrawal_output()].into(),
            ..Default::default()
        };
        assert_eq!(
            tx.txid().to_string(),
            "67a547c70a329e8c76025dd41bebe60b8f3066628ec90f232ed412e4ba56f50b"
        );
    }

    /// Canonical encoding of a game deposit output: the 20-byte address (all zeros), tag 2, the value as a
    /// little-endian u64, the 32 account bytes. 61 bytes.
    #[test]
    fn game_deposit_canonical_encoding() {
        let output = game_deposit_output();
        let expected = format!(
            "{}02{}{}",
            "00".repeat(20),
            "8813000000000000",
            ACCOUNT_HEX
        );
        assert_eq!(hex(&output), expected);
        assert_eq!(borsh::to_vec(&output).unwrap().len(), 61);
        assert_eq!(output.canonical_size().unwrap(), 61);
        // the content alone: tag, value, account
        assert_eq!(
            hex(&output.content),
            format!("028813000000000000{ACCOUNT_HEX}")
        );
        // it counts toward a transaction's outputs like any other
        assert_eq!(output.get_value(), bitcoin::Amount::from_sat(5000));
        assert!(output.content.is_game_deposit());
        assert!(!output.content.is_value());
        assert!(!output.content.is_withdrawal());
    }

    #[test]
    fn game_deposit_json() {
        let output = game_deposit_output();
        let json = serde_json::to_string(&output).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"address":"11111111111111111111","content":{{"GameDeposit":{{"value":5000,"account":"{ACCOUNT_HEX}"}}}}}}"#
            )
        );
        assert_eq!(serde_json::from_str::<Output>(&json).unwrap(), output);
        // upper case hex reads as the same account
        let upper = json.replace(ACCOUNT_HEX, &ACCOUNT_HEX.to_uppercase());
        assert_eq!(serde_json::from_str::<Output>(&upper).unwrap(), output);
        // anything but 64 hex digits is refused
        for bad in [
            &ACCOUNT_HEX[..62],
            &format!("0x{}", &ACCOUNT_HEX[..62]),
            &format!("{ACCOUNT_HEX}00"),
            &format!("zz{}", &ACCOUNT_HEX[2..]),
        ] {
            let bad = json.replace(ACCOUNT_HEX, bad);
            assert!(serde_json::from_str::<Output>(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn transaction_lists_its_game_deposits_in_output_order() {
        let other = [0x22; 32];
        let deposit = |sats, account| Output {
            address: Address::ALL_ZEROS,
            content: Content::GameDeposit {
                value: bitcoin::Amount::from_sat(sats),
                account,
            },
        };
        let tx = Transaction {
            outputs: vec![
                value_output(),
                deposit(3, other),
                withdrawal_output(),
                deposit(1, ACCOUNT),
                deposit(2, other),
            ]
            .into(),
            ..Default::default()
        };
        assert!(tx.has_game_deposit());
        let sat = bitcoin::Amount::from_sat;
        assert_eq!(
            tx.game_deposits().collect::<Vec<_>>(),
            vec![(other, sat(3)), (ACCOUNT, sat(1)), (other, sat(2))]
        );
        let plain = Transaction {
            outputs: vec![value_output(), withdrawal_output()].into(),
            ..Default::default()
        };
        assert!(!plain.has_game_deposit());
        assert_eq!(plain.game_deposits().count(), 0);
    }
}
