//! Ordinal transaction construction is fraught.
//!
//! Ordinal-aware transaction construction has additional invariants,
//! constraints, and concerns in addition to those of normal, non-ordinal-aware
//! Bitcoin transactions.
//!
//! This module contains a `TransactionBuilder` struct that facilitates
//! constructing ordinal-aware transactions that take these additional
//! conditions into account.
//!
//! The external interfaces are
//! `TransactionBuilder::build_transaction_with_postage`, and
//! `TransactionBuilder::build_transaction_with_value`. Both return a
//! constructed transaction given the arguments, which include the outgoing sat
//! to send, the wallets current UTXOs and their sat ranges, and the
//! recipient's address.
//!
//! `TransactionBuilder::build_transaction_with_postage` ensures that the
//! outgoing value is at most 20,000 sats, reducing it to 10,000 sats if coin
//! selection requires adding excess value.
//!
//! `TransactionBuilder::build_transaction_with_value` ensures that the
//! outgoing value is exactly the requested amount,
//!
//! Internally, `TransactionBuilder` calls multiple methods that implement
//! transformations responsible for individual concerns, such as ensuring that
//! the transaction fee is paid, and that outgoing outputs aren't too large.
//!
//! This module is heavily tested. For all features of transaction
//! construction, there should be a positive test that checks that the feature
//! is implemented correctly, an assertion in the final `Transaction::build`
//! method that the built transaction is correct with respect to the feature,
//! and a test that the assertion fires as expected.

use {
  super::*,
  bitcoin::{
    blockdata::{locktime::PackedLockTime, witness::Witness},
    util::amount::Amount,
  },
  std::collections::{BTreeMap, BTreeSet},
};

#[derive(Debug, PartialEq)]
pub enum Error {
  DuplicateAddress(Address),
  Dust {
    output_value: Amount,
    dust_value: Amount,
  },
  MaxPostageLessThanTarget {
    max_postage: Amount,
    target_postage: Amount,
  },
  NotEnoughCardinalUtxos,
  NotInWallet(SatPoint),
  OutOfRange(SatPoint, u64),
  TooManyInputs(usize),
  UtxoContainsAdditionalInscription {
    outgoing_satpoint: SatPoint,
    inscribed_satpoint: SatPoint,
    inscription_id: InscriptionId,
  },
  ValueOverflow,
}

#[derive(Debug, PartialEq)]
enum Target {
  Value(Amount),
  Postage,
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::Dust {
        output_value,
        dust_value,
      } => write!(f, "output value is below dust value: {output_value} < {dust_value}"),
      Error::MaxPostageLessThanTarget {max_postage, target_postage} => write!(f, "max postage {} sats is less than target postage {} sats", max_postage.to_sat(), target_postage.to_sat()),
      Error::NotInWallet(outgoing_satpoint) => write!(f, "outgoing satpoint {outgoing_satpoint} not in wallet"),
      Error::OutOfRange(outgoing_satpoint, maximum) => write!(f, "outgoing satpoint {outgoing_satpoint} offset higher than maximum {maximum}"),
      Error::TooManyInputs(max_inputs) => write!(f, "--max-inputs ({max_inputs}) exceeded"),
      Error::NotEnoughCardinalUtxos => write!(
        f,
        "wallet does not contain enough cardinal UTXOs, please add additional funds to wallet."
      ),
      Error::UtxoContainsAdditionalInscription {
        outgoing_satpoint,
        inscribed_satpoint,
        inscription_id,
      } => write!(
        f,
        "cannot send {outgoing_satpoint} without also sending inscription {inscription_id} at {inscribed_satpoint}"
      ),
      Error::ValueOverflow => write!(f, "arithmetic overflow calculating value"),
      Error::DuplicateAddress(address) => write!(f, "duplicate input address: {address}"),
    }
  }
}

impl std::error::Error for Error {}

#[derive(Debug)]
pub struct TransactionBuilder {
  amounts: BTreeMap<OutPoint, Amount>,
  change_addresses: BTreeSet<Address>,
  fee_rate: FeeRate,
  max_inputs: Option<usize>,
  inputs: Vec<OutPoint>,
  inscriptions: BTreeMap<SatPoint, InscriptionId>,
  outgoing: SatPoint,
  outputs: Vec<(Address, Amount)>,
  recipient: Vec<Address>,
  alignment: Option<Address>,
  unused_change_addresses: Vec<Address>,
  utxos: BTreeSet<OutPoint>,
  target: Vec<Target>,
  target_postage: Amount,
  max_postage: Amount,
  current_output: usize,
  padding_outputs: usize,
}

type Result<T> = std::result::Result<T, Error>;

impl TransactionBuilder {
  const ADDITIONAL_INPUT_VBYTES: f64 = 58.0;
  const ADDITIONAL_OUTPUT_VBYTES: f64 = 43.0;
  const SCHNORR_SIGNATURE_SIZE: usize = 64;
  pub(crate) const DEFAULT_MAX_POSTAGE: Amount = Amount::from_sat(2 * 10_000);
  pub(crate) const DEFAULT_TARGET_POSTAGE: Amount = Amount::from_sat(10_000);

  pub fn build_transaction_with_postage(
    outgoing: SatPoint,
    inscriptions: BTreeMap<SatPoint, InscriptionId>,
    amounts: BTreeMap<OutPoint, Amount>,
    recipient: Address,
    alignment: Option<Address>,
    change: [Address; 2],
    fee_rate: FeeRate,
    max_inputs: Option<usize>,
    target_postage: Amount,
    max_postage: Amount,
  ) -> Result<Transaction> {
    if max_postage < target_postage {
      return Err(Error::MaxPostageLessThanTarget {
        max_postage,
        target_postage,
      });
    }

    Self::new(
      outgoing,
      inscriptions,
      amounts,
      vec![recipient],
      alignment,
      change,
      fee_rate,
      max_inputs,
      vec![Target::Postage],
      target_postage,
      max_postage,
    )?
    .build_transaction()
  }

  pub fn build_transaction_with_value(
    outgoing: SatPoint,
    inscriptions: BTreeMap<SatPoint, InscriptionId>,
    amounts: BTreeMap<OutPoint, Amount>,
    recipient: Address,
    alignment: Option<Address>,
    change: [Address; 2],
    fee_rate: FeeRate,
    max_inputs: Option<usize>,
    output_value: Amount,
  ) -> Result<Transaction> {
    let dust_value = recipient.script_pubkey().dust_value();

    if output_value < dust_value {
      return Err(Error::Dust {
        output_value,
        dust_value,
      });
    }

    Self::new(
      outgoing,
      inscriptions,
      amounts,
      vec![recipient],
      alignment,
      change,
      fee_rate,
      max_inputs,
      vec![Target::Value(output_value)],
      Amount::from_sat(0),
      Amount::from_sat(0),
    )?
    .build_transaction()
  }

  pub fn build_transaction_with_values(
    outgoing: SatPoint,
    inscriptions: BTreeMap<SatPoint, InscriptionId>,
    amounts: BTreeMap<OutPoint, Amount>,
    recipient: Vec<Address>,
    alignment: Option<Address>,
    change: [Address; 2],
    fee_rate: FeeRate,
    output_value: Vec<Amount>,
    max_inputs: Option<usize>,
  ) -> Result<Transaction> {
    for (recipient, output_value) in recipient.iter().zip(output_value.clone()) {
      let dust_value = recipient.script_pubkey().dust_value();

      if output_value < dust_value {
        return Err(Error::Dust {
          output_value,
          dust_value,
        });
      }
    }

    Self::new(
      outgoing,
      inscriptions,
      amounts,
      recipient,
      alignment,
      change,
      fee_rate,
      max_inputs,
      output_value
        .iter()
        .map(|output_value| Target::Value(*output_value))
        .collect(),
      Amount::from_sat(0),
      Amount::from_sat(0),
    )?
    .build_transaction()
  }

  fn build_transaction(self) -> Result<Transaction> {
    self
      .select_outgoing()?
      .align_outgoing()
      .pad_alignment_output()?
      .add_value()?
      .strip_value()
      .deduct_fee()
      .build()
  }

  fn new(
    outgoing: SatPoint,
    inscriptions: BTreeMap<SatPoint, InscriptionId>,
    amounts: BTreeMap<OutPoint, Amount>,
    recipient: Vec<Address>,
    alignment: Option<Address>,
    change: [Address; 2],
    fee_rate: FeeRate,
    max_inputs: Option<usize>,
    target: Vec<Target>,
    target_postage: Amount,
    max_postage: Amount,
  ) -> Result<Self> {
    for recipient in recipient.clone() {
      if change.contains(&recipient) {
        return Err(Error::DuplicateAddress(recipient));
      }
    }

    if change[0] == change[1] {
      return Err(Error::DuplicateAddress(change[0].clone()));
    }

    if let Some(max_inputs) = max_inputs {
      if max_inputs < 1 {
        return Err(Error::TooManyInputs(max_inputs));
      }
    }

    Ok(Self {
      utxos: amounts.keys().cloned().collect(),
      amounts,
      change_addresses: change.iter().cloned().collect(),
      fee_rate,
      max_inputs,
      inputs: Vec::new(),
      inscriptions,
      outgoing,
      outputs: Vec::new(),
      recipient,
      alignment,
      unused_change_addresses: change.to_vec(),
      target,
      target_postage,
      max_postage,
      current_output: 0,
      padding_outputs: 0,
    })
  }

  fn select_outgoing(mut self) -> Result<Self> {
    let dust_limit = self
      .unused_change_addresses
      .last()
      .unwrap()
      .script_pubkey()
      .dust_value()
      .to_sat();
    for (inscribed_satpoint, inscription_id) in self.inscriptions.iter().rev() {
      if self.outgoing.outpoint == inscribed_satpoint.outpoint
        && self.outgoing.offset != inscribed_satpoint.offset
        && (self.outgoing.offset < inscribed_satpoint.offset
          || (self.outgoing.offset > 0 && self.outgoing.offset < dust_limit))
      {
        return Err(Error::UtxoContainsAdditionalInscription {
          outgoing_satpoint: self.outgoing,
          inscribed_satpoint: *inscribed_satpoint,
          inscription_id: *inscription_id,
        });
      }
    }

    let amount = *self
      .amounts
      .get(&self.outgoing.outpoint)
      .ok_or(Error::NotInWallet(self.outgoing))?;

    if self.outgoing.offset >= amount.to_sat() {
      return Err(Error::OutOfRange(self.outgoing, amount.to_sat() - 1));
    }

    self.utxos.remove(&self.outgoing.outpoint);
    self.inputs.push(self.outgoing.outpoint);
    let mut available = amount;
    for (recipient, target) in self.recipient.iter().zip(self.target.iter()) {
      let value = match target {
        Target::Postage => recipient.script_pubkey().dust_value(),
        Target::Value(value) => *value,
      };
      if available < value {
        self.outputs.push((recipient.clone(), available));
        available = Amount::from_sat(0);
      } else {
        if self.current_output == self.recipient.len() - 1 {
          self.outputs.push((recipient.clone(), available));
          break;
        } else {
          self.outputs.push((recipient.clone(), value));
          available -= value;
        }
        self.current_output += 1;
      }
    }

    tprintln!(
      "selected outgoing outpoint {} with value {}",
      self.outgoing.outpoint,
      amount.to_sat()
    );

    Ok(self)
  }

  fn align_outgoing(mut self) -> Self {
    assert_eq!(
      self.outputs.len(),
      self.recipient.len(),
      "invariant: same number of outputs as recipients"
    );

    assert_eq!(
      self.outputs[0].0, self.recipient[0],
      "invariant: first output is recipient"
    );

    let sat_offset = self.calculate_sat_offset();
    if sat_offset == 0 {
      tprintln!("outgoing is aligned");
    } else {
      tprintln!("aligned outgoing with {sat_offset} sat padding output");
      self.outputs.insert(
        0,
        (
          match self.alignment.clone() {
            Some(alignment) => alignment,
            None => self.unused_change_addresses[0].clone(),
          },
          Amount::from_sat(sat_offset),
        ),
      );
      self.padding_outputs = 1;
      let mut debit = Amount::from_sat(sat_offset);
      loop {
        if self.outputs[self.current_output + self.padding_outputs].1 < debit {
          debit -= self.outputs[self.current_output + self.padding_outputs].1;
          self.outputs[self.current_output + self.padding_outputs].1 = Amount::from_sat(0);
          self.current_output -= 1;
        } else {
          self.outputs[self.current_output + self.padding_outputs].1 -= debit;
          break;
        }
      }
    }

    self
  }

  fn pad_alignment_output(mut self) -> Result<Self> {
    if self.outputs[0].0 == self.recipient[0] {
      tprintln!("no alignment output");
    } else {
      let dust_limit = self
        .unused_change_addresses
        .last()
        .unwrap()
        .script_pubkey()
        .dust_value();
      if self.outputs[0].1 >= dust_limit {
        tprintln!("no padding needed");
      } else {
        while self.outputs[0].1 < dust_limit {
          let (utxo, size) = self.select_cardinal_utxo(dust_limit - self.outputs[0].1, true)?; // prefer smaller utxos to tidy dust outputs
          self.inputs.insert(0, utxo);
          self.outputs[0].1 += size;
          tprintln!(
            "padded alignment output to {} with additional {size} sat input",
            self.outputs[0].1
          );
        }
      }
    }

    Ok(self)
  }

  fn add_value(mut self) -> Result<Self> {
    let estimated_fee = self.estimate_fee();

    let min_value = self
      .recipient
      .iter()
      .zip(self.target.iter())
      .map(|(recipient, target)| match target {
        Target::Postage => recipient.script_pubkey().dust_value(),
        Target::Value(value) => *value,
      })
      .sum::<Amount>();

    let total = min_value
      .checked_add(estimated_fee)
      .ok_or(Error::ValueOverflow)?;

    let have = if self.padding_outputs == 0 {
      self.outputs.clone().iter().map(|output| output.1).sum()
    } else {
      self
        .outputs
        .clone()
        .iter()
        .skip(1)
        .map(|output| output.1)
        .sum()
    };
    if let Some(mut deficit) = total.checked_sub(have) {
      while deficit > Amount::ZERO {
        let additional_fee = self.fee_rate.fee(Self::ADDITIONAL_INPUT_VBYTES);
        let needed = deficit
          .checked_add(additional_fee)
          .ok_or(Error::ValueOverflow)?;
        let (utxo, value) = self.select_cardinal_utxo(needed, false)?; // prefer utxos that fill the needed amount
        let benefit = value
          .checked_sub(additional_fee)
          .ok_or(Error::NotEnoughCardinalUtxos)?;
        self.inputs.push(utxo);
        let mut available = value;

        while self.current_output < self.recipient.len() {
          let demand = match self.target[self.current_output] {
            Target::Postage => self.recipient[self.current_output]
              .script_pubkey()
              .dust_value(),
            Target::Value(value) => value,
          };
          let existing = self.outputs[self.current_output + self.padding_outputs].1;
          if available.checked_add(existing) < Some(demand) {
            self.outputs[self.current_output + self.padding_outputs].1 += available;
            break;
          } else {
            if self.current_output == self.recipient.len() - 1 {
              self.outputs[self.current_output + self.padding_outputs].1 += available;
              break;
            } else {
              self.outputs[self.current_output + self.padding_outputs].1 += demand - existing;
              available -= demand - existing;
            }
            self.current_output += 1;
          }
        }

        if benefit > deficit {
          tprintln!("added {value} sat input to cover {deficit} sat deficit");
          deficit = Amount::ZERO;
        } else {
          tprintln!("added {value} sat input to reduce {deficit} sat deficit by {benefit} sat");
          deficit -= benefit;
        }
      }
    }

    Ok(self)
  }

  fn strip_value(mut self) -> Self {
    let sat_offset = self.calculate_sat_offset();

    let total_output_amount = self
      .outputs
      .iter()
      .map(|(_address, amount)| *amount)
      .sum::<Amount>();

    for recipient in self.recipient.iter() {
      self
        .outputs
        .iter()
        .find(|(address, _amount)| *address == *recipient)
        .expect("couldn't find output that contains the index");
    }

    let value = total_output_amount - Amount::from_sat(sat_offset);
    if let Some(excess) = value.checked_sub(self.fee_rate.fee(self.estimate_vbytes())) {
      let (max, target) = self
        .target
        .iter()
        .map(|target| match target {
          Target::Postage => (self.max_postage, self.target_postage),
          Target::Value(value) => (*value, *value),
        })
        .reduce(|(a, b), (c, d)| (a + c, b + d))
        .unwrap();

      if excess > max
        && value.checked_sub(target).unwrap()
          > self
            .unused_change_addresses
            .last()
            .unwrap()
            .script_pubkey()
            .dust_value()
            + self
              .fee_rate
              .fee(self.estimate_vbytes() + Self::ADDITIONAL_OUTPUT_VBYTES)
      {
        tprintln!("stripped {} sats", (value - target).to_sat());
        self.outputs.last_mut().expect("no outputs found").1 -= value - target;
        self
          .outputs
          .push((self.unused_change_addresses[1].clone(), value - target));
      }
    }

    self
  }

  fn deduct_fee(mut self) -> Self {
    let sat_offset = self.calculate_sat_offset();

    let fee = self.estimate_fee();

    let total_output_amount = self
      .outputs
      .iter()
      .map(|(_address, amount)| *amount)
      .sum::<Amount>();

    let (_address, last_output_amount) = self
      .outputs
      .last_mut()
      .expect("No output to deduct fee from");

    assert!(
      total_output_amount.checked_sub(fee).unwrap() > Amount::from_sat(sat_offset),
      "invariant: deducting fee does not consume sat",
    );

    assert!(
      *last_output_amount >= fee,
      "invariant: last output can pay fee: {} {}",
      *last_output_amount,
      fee,
    );

    *last_output_amount -= fee;

    self
  }

  /// Estimate the size in virtual bytes of the transaction under construction.
  /// We initialize wallets with taproot descriptors only, so we know that all
  /// inputs are taproot key path spends, which allows us to know that witnesses
  /// will all consist of single Schnorr signatures.
  fn estimate_vbytes(&self) -> f64 {
    Self::estimate_vbytes_with(
      self.inputs.len(),
      self
        .outputs
        .iter()
        .map(|(address, _amount)| address)
        .cloned()
        .collect(),
    )
  }

  fn estimate_vbytes_with(inputs: usize, outputs: Vec<Address>) -> f64 {
    let t = Transaction {
      version: 1,
      lock_time: PackedLockTime::ZERO,
      input: (0..inputs)
        .map(|_| TxIn {
          previous_output: OutPoint::null(),
          script_sig: Script::new(),
          sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
          witness: Witness::from_vec(vec![vec![0; TransactionBuilder::SCHNORR_SIGNATURE_SIZE]]),
        })
        .collect(),
      output: outputs
        .into_iter()
        .map(|address| TxOut {
          value: 0,
          script_pubkey: address.script_pubkey(),
        })
        .collect(),
    };

    t.weight() as f64 / 4.0
  }

  fn estimate_fee(&self) -> Amount {
    // println!("size {} weight {}", self.estimate_vbytes(),
    self.fee_rate.fee(self.estimate_vbytes())
  }

  fn build(self) -> Result<Transaction> {
    let recipient: Vec<Script> = self
      .recipient
      .iter()
      .map(|recipient| recipient.script_pubkey())
      .collect();
    let transaction = Transaction {
      version: 1,
      lock_time: PackedLockTime::ZERO,
      input: self
        .inputs
        .iter()
        .map(|outpoint| TxIn {
          previous_output: *outpoint,
          script_sig: Script::new(),
          sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
          witness: Witness::new(),
        })
        .collect(),
      output: self
        .outputs
        .iter()
        .map(|(address, amount)| TxOut {
          value: amount.to_sat(),
          script_pubkey: address.script_pubkey(),
        })
        .collect(),
    };

    assert_eq!(
      self
        .amounts
        .iter()
        .filter(|(outpoint, amount)| *outpoint == &self.outgoing.outpoint
          && self.outgoing.offset < amount.to_sat())
        .count(),
      1,
      "invariant: outgoing sat is contained in utxos"
    );

    assert_eq!(
      transaction
        .input
        .iter()
        .filter(|tx_in| tx_in.previous_output == self.outgoing.outpoint)
        .count(),
      1,
      "invariant: inputs spend outgoing sat"
    );

    let mut sat_offset = 0;
    let mut found = false;
    for tx_in in &transaction.input {
      if tx_in.previous_output == self.outgoing.outpoint {
        sat_offset += self.outgoing.offset;
        found = true;
        break;
      } else {
        sat_offset += self.amounts[&tx_in.previous_output].to_sat();
      }
    }
    assert!(found, "invariant: outgoing sat is found in inputs");

    let mut output_end = 0;
    let mut found = false;
    for tx_out in &transaction.output {
      output_end += tx_out.value;
      if output_end > sat_offset {
        assert_eq!(
          tx_out.script_pubkey, recipient[0],
          "invariant: outgoing sat is sent to first recipient"
        );
        found = true;
        break;
      }
    }
    assert!(found, "invariant: outgoing sat is found in outputs");

    for recipient in self.recipient.iter() {
      assert_eq!(
        transaction
          .output
          .iter()
          .filter(|tx_out| tx_out.script_pubkey == recipient.script_pubkey())
          .count(),
        1,
        "invariant: recipient address appears exactly once in outputs",
      );
    }

    assert!(
      self
        .change_addresses
        .iter()
        .map(|change_address| transaction
          .output
          .iter()
          .filter(|tx_out| tx_out.script_pubkey == change_address.script_pubkey())
          .count())
        .all(|count| count <= 1),
      "invariant: change addresses appear at most once in outputs",
    );

    let mut offset = 0;
    for output in &transaction.output {
      if output.script_pubkey == self.recipient[0].script_pubkey() {
        assert_eq!(
          offset, sat_offset,
          "invariant: sat is at first position in first recipient output"
        );
        break;
      }
      offset += output.value;
    }

    let slop = self.fee_rate.fee(Self::ADDITIONAL_OUTPUT_VBYTES);
    let mut n = self.padding_outputs;
    for (recipient, target) in self.recipient.iter().zip(self.target) {
      let output = &transaction.output[n];
      assert_eq!(output.script_pubkey, recipient.script_pubkey());

      match target {
        Target::Postage => {
          assert!(
            Amount::from_sat(output.value) <= self.max_postage + slop,
            "invariant: excess postage is stripped"
          );
        }
        Target::Value(value) => {
          assert!(
            Amount::from_sat(output.value).checked_sub(value).unwrap()
              <= self
                .change_addresses
                .iter()
                .map(|address| address.script_pubkey().dust_value())
                .max()
                .unwrap_or_default()
                + slop,
            "invariant: output equals target value",
          );
        }
      }
      n += 1;
    }

    for (i, output) in transaction.output.iter().enumerate() {
      if (i < self.padding_outputs || i >= self.padding_outputs + self.recipient.len())
        && (self.alignment.is_none()
          || (self.alignment.is_some()
            && output.script_pubkey != self.alignment.as_ref().unwrap().script_pubkey()))
      {
        assert!(
          self
            .change_addresses
            .iter()
            .any(|change_address| change_address.script_pubkey() == output.script_pubkey),
          "invariant: all outputs are either change or recipient: unrecognized output {} {}",
          output.script_pubkey,
          i
        );
      }
    }

    let mut actual_fee = Amount::ZERO;
    for input in &transaction.input {
      actual_fee += self.amounts[&input.previous_output];
    }
    for output in &transaction.output {
      actual_fee -= Amount::from_sat(output.value);
    }

    let mut modified_tx = transaction.clone();
    for input in &mut modified_tx.input {
      input.witness = Witness::from_vec(vec![vec![0; 64]]);
    }
    let expected_fee = self.fee_rate.fee(modified_tx.weight() as f64 / 4.0);

    assert_eq!(
      actual_fee, expected_fee,
      "invariant: fee estimation is correct",
    );

    for tx_out in &transaction.output {
      assert!(
        Amount::from_sat(tx_out.value) >= tx_out.script_pubkey.dust_value(),
        "invariant: all outputs are above dust limit",
      );
    }

    Ok(transaction)
  }

  fn calculate_sat_offset(&self) -> u64 {
    let mut sat_offset = 0;
    for outpoint in &self.inputs {
      if *outpoint == self.outgoing.outpoint {
        return sat_offset + self.outgoing.offset;
      } else {
        sat_offset += self.amounts[outpoint].to_sat();
      }
    }

    panic!("Could not find outgoing sat in inputs");
  }

  fn select_cardinal_utxo(
    &mut self,
    target_value: Amount,
    prefer_under: bool,
  ) -> Result<(OutPoint, Amount)> {
    let mut found = None;
    let mut best = Amount::ZERO;

    if self.max_inputs.is_some() && self.inputs.len() == self.max_inputs.unwrap() {
      return Err(Error::TooManyInputs(self.max_inputs.unwrap()));
    }

    tprintln!(
      "looking for {} cardinal worth {target_value}",
      if prefer_under { "smaller" } else { "bigger" }
    );

    let inscribed_utxos = self
      .inscriptions
      .keys()
      .map(|satpoint| satpoint.outpoint)
      .collect::<BTreeSet<OutPoint>>();

    for utxo in &self.utxos {
      if inscribed_utxos.contains(utxo) {
        continue;
      }

      let value = self.amounts[utxo];

      if prefer_under {
        // prefer an output smaller than the target over one bigger than it
        if best == Amount::ZERO {
          found = Some((*utxo, value));
          best = value;
        } else if best <= target_value {
          if value <= target_value && value > best {
            found = Some((*utxo, value));
            best = value;
          }
        } else if value <= target_value || value < best {
          found = Some((*utxo, value));
          best = value;
        }
      } else {
        // prefer an output bigger than the target over one smaller than it
        if best >= target_value {
          if value >= target_value && value < best {
            found = Some((*utxo, value));
            best = value;
          }
        } else if value >= target_value || value > best {
          found = Some((*utxo, value));
          best = value;
        }
      }
    }

    let (utxo, value) = found.ok_or(Error::NotEnoughCardinalUtxos)?;

    self.utxos.remove(&utxo);
    tprintln!("found cardinal worth {}", value);

    Ok((utxo, value))
  }
}

#[cfg(test)]
mod tests {
  use {super::Error, super::*};

  #[test]
  fn select_sat() {
    let mut utxos = vec![
      (outpoint(1), Amount::from_sat(5_000)),
      (outpoint(2), Amount::from_sat(49 * COIN_VALUE)),
      (outpoint(3), Amount::from_sat(2_000)),
    ];

    let tx_builder = TransactionBuilder::new(
      satpoint(2, 0),
      BTreeMap::new(),
      utxos.clone().into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap();

    utxos.remove(1);
    assert_eq!(
      tx_builder.utxos,
      utxos.iter().map(|(outpoint, _ranges)| *outpoint).collect()
    );
    assert_eq!(tx_builder.inputs, [outpoint(2)]);
    assert_eq!(
      tx_builder.outputs,
      [(
        recipient(),
        Amount::from_sat(100 * COIN_VALUE - 51 * COIN_VALUE)
      )]
    )
  }

  #[test]
  fn tx_builder_to_transaction() {
    let mut amounts = BTreeMap::new();
    amounts.insert(outpoint(1), Amount::from_sat(5_000));
    amounts.insert(outpoint(2), Amount::from_sat(5_000));
    amounts.insert(outpoint(3), Amount::from_sat(2_000));

    let tx_builder = TransactionBuilder {
      amounts,
      fee_rate: FeeRate::try_from(1.0).unwrap(),
      max_inputs: None,
      utxos: BTreeSet::new(),
      outgoing: satpoint(1, 0),
      inscriptions: BTreeMap::new(),
      recipient: vec![recipient()],
      alignment: None,
      unused_change_addresses: vec![change(0), change(1)],
      change_addresses: vec![change(0), change(1)].into_iter().collect(),
      inputs: vec![outpoint(1), outpoint(2), outpoint(3)],
      outputs: vec![
        (recipient(), Amount::from_sat(5_000)),
        (change(0), Amount::from_sat(5_000)),
        (change(1), Amount::from_sat(1_724)),
      ],
      target: vec![Target::Postage],
      target_postage: TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      max_postage: TransactionBuilder::DEFAULT_MAX_POSTAGE,
      current_output: 0,
      padding_outputs: 0,
    };

    pretty_assert_eq!(
      tx_builder.build(),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1)), tx_in(outpoint(2)), tx_in(outpoint(3))],
        output: vec![
          tx_out(5_000, recipient()),
          tx_out(5_000, change(0)),
          tx_out(1_724, change(1))
        ],
      })
    )
  }

  #[test]
  fn transactions_are_rbf() {
    let utxos = vec![(outpoint(1), Amount::from_sat(5_000))];

    assert!(TransactionBuilder::build_transaction_with_postage(
      satpoint(1, 0),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      recipient(),
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .is_explicitly_rbf())
  }

  #[test]
  fn deduct_fee() {
    let utxos = vec![(outpoint(1), Amount::from_sat(5_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(4901, recipient())],
      })
    )
  }

  #[test]
  #[should_panic(expected = "invariant: deducting fee does not consume sat")]
  fn invariant_deduct_fee_does_not_consume_sat() {
    let utxos = vec![(outpoint(1), Amount::from_sat(5_000))];

    TransactionBuilder::new(
      satpoint(1, 4_950),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .align_outgoing()
    .strip_value()
    .deduct_fee();
  }

  #[test]
  fn additional_postage_added_when_required() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(5_000)),
      (outpoint(2), Amount::from_sat(5_000)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 4_950),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1)), tx_in(outpoint(2))],
        output: vec![tx_out(4_950, change(0)), tx_out(4_862, recipient())],
      })
    )
  }

  #[test]
  fn insufficient_padding_to_add_postage_no_utxos() {
    let utxos = vec![(outpoint(1), Amount::from_sat(5_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 4_950),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Err(Error::NotEnoughCardinalUtxos),
    )
  }

  #[test]
  fn insufficient_padding_to_add_postage_small_utxos() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(5_000)),
      (outpoint(2), Amount::from_sat(1)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 4_950),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Err(Error::NotEnoughCardinalUtxos),
    )
  }

  #[test]
  fn excess_additional_postage_is_stripped() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(5_000)),
      (outpoint(2), Amount::from_sat(25_000)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 4_950),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1)), tx_in(outpoint(2))],
        output: vec![
          tx_out(4_950, change(0)),
          tx_out(
            TransactionBuilder::DEFAULT_TARGET_POSTAGE.to_sat(),
            recipient()
          ),
          tx_out(14_831, change(1)),
        ],
      })
    )
  }

  #[test]
  #[should_panic(expected = "invariant: outgoing sat is contained in utxos")]
  fn invariant_satpoint_outpoint_is_contained_in_utxos() {
    TransactionBuilder::new(
      satpoint(2, 0),
      BTreeMap::new(),
      vec![(outpoint(1), Amount::from_sat(4))]
        .into_iter()
        .collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: outgoing sat is contained in utxos")]
  fn invariant_satpoint_offset_is_contained_in_utxos() {
    TransactionBuilder::new(
      satpoint(1, 4),
      BTreeMap::new(),
      vec![(outpoint(1), Amount::from_sat(4))]
        .into_iter()
        .collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: inputs spend outgoing sat")]
  fn invariant_inputs_spend_sat() {
    TransactionBuilder::new(
      satpoint(1, 2),
      BTreeMap::new(),
      vec![(outpoint(1), Amount::from_sat(5))]
        .into_iter()
        .collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: outgoing sat is sent to first recipient")]
  fn invariant_sat_is_sent_to_recipient() {
    let mut builder = TransactionBuilder::new(
      satpoint(1, 2),
      BTreeMap::new(),
      vec![(outpoint(1), Amount::from_sat(5))]
        .into_iter()
        .collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap();

    builder.outputs[0].0 = "tb1qx4gf3ya0cxfcwydpq8vr2lhrysneuj5d7lqatw"
      .parse()
      .unwrap();

    builder.build().unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: outgoing sat is found in outputs")]
  fn invariant_sat_is_found_in_outputs() {
    let mut builder = TransactionBuilder::new(
      satpoint(1, 2),
      BTreeMap::new(),
      vec![(outpoint(1), Amount::from_sat(5))]
        .into_iter()
        .collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap();

    builder.outputs[0].1 = Amount::from_sat(0);

    builder.build().unwrap();
  }

  #[test]
  fn excess_postage_is_stripped() {
    let utxos = vec![(outpoint(1), Amount::from_sat(1_000_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![
          tx_out(
            TransactionBuilder::DEFAULT_TARGET_POSTAGE.to_sat(),
            recipient()
          ),
          tx_out(989_870, change(1))
        ],
      })
    )
  }

  #[test]
  #[should_panic(expected = "invariant: excess postage is stripped")]
  fn invariant_excess_postage_is_stripped() {
    let utxos = vec![(outpoint(1), Amount::from_sat(1_000_000))];

    TransactionBuilder::new(
      satpoint(1, 0),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .build()
    .unwrap();
  }

  #[test]
  fn sat_is_aligned() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 3_333),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(3_333, change(0)), tx_out(6_537, recipient())],
      })
    )
  }

  #[test]
  fn alignment_output_under_dust_limit_is_padded() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(10_000)),
      (outpoint(2), Amount::from_sat(10_000)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 1),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        None,
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(2)), tx_in(outpoint(1))],
        output: vec![tx_out(10_001, change(0)), tx_out(9_811, recipient())],
      })
    )
  }

  #[test]
  #[should_panic(expected = "invariant: all outputs are either change or recipient")]
  fn invariant_all_output_are_recognized() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    let mut builder = TransactionBuilder::new(
      satpoint(1, 3_333),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .align_outgoing()
    .add_value()
    .unwrap()
    .strip_value()
    .deduct_fee();

    builder.change_addresses = BTreeSet::new();

    builder.build().unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: all outputs are above dust limit")]
  fn invariant_all_output_are_above_dust_limit() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    TransactionBuilder::new(
      satpoint(1, 1),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .align_outgoing()
    .add_value()
    .unwrap()
    .strip_value()
    .deduct_fee()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: sat is at first position in first recipient output")]
  fn invariant_sat_is_aligned() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    TransactionBuilder::new(
      satpoint(1, 3_333),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .strip_value()
    .deduct_fee()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: fee estimation is correct")]
  fn invariant_fee_is_at_least_target_fee_rate() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    TransactionBuilder::new(
      satpoint(1, 0),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Postage],
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .strip_value()
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: recipient address appears exactly once in outputs")]
  fn invariant_recipient_appears_exactly_once() {
    let mut amounts = BTreeMap::new();
    amounts.insert(outpoint(1), Amount::from_sat(5_000));
    amounts.insert(outpoint(2), Amount::from_sat(5_000));
    amounts.insert(outpoint(3), Amount::from_sat(2_000));

    TransactionBuilder {
      amounts,
      fee_rate: FeeRate::try_from(1.0).unwrap(),
      max_inputs: None,
      utxos: BTreeSet::new(),
      outgoing: satpoint(1, 0),
      inscriptions: BTreeMap::new(),
      recipient: vec![recipient()],
      alignment: alignment(),
      unused_change_addresses: vec![change(0), change(1)],
      change_addresses: vec![change(0), change(1)].into_iter().collect(),
      inputs: vec![outpoint(1), outpoint(2), outpoint(3)],
      outputs: vec![
        (recipient(), Amount::from_sat(5_000)),
        (recipient(), Amount::from_sat(5_000)),
        (change(1), Amount::from_sat(1_774)),
      ],
      target: vec![Target::Postage],
      target_postage: TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      max_postage: TransactionBuilder::DEFAULT_MAX_POSTAGE,
      current_output: 0,
      padding_outputs: 0,
    }
    .build()
    .unwrap();
  }

  #[test]
  #[should_panic(expected = "invariant: change addresses appear at most once in outputs")]
  fn invariant_change_appears_at_most_once() {
    let mut amounts = BTreeMap::new();
    amounts.insert(outpoint(1), Amount::from_sat(5_000));
    amounts.insert(outpoint(2), Amount::from_sat(5_000));
    amounts.insert(outpoint(3), Amount::from_sat(2_000));

    TransactionBuilder {
      amounts,
      fee_rate: FeeRate::try_from(1.0).unwrap(),
      max_inputs: None,
      utxos: BTreeSet::new(),
      outgoing: satpoint(1, 0),
      inscriptions: BTreeMap::new(),
      recipient: vec![recipient()],
      alignment: alignment(),
      unused_change_addresses: vec![change(0), change(1)],
      change_addresses: vec![change(0), change(1)].into_iter().collect(),
      inputs: vec![outpoint(1), outpoint(2), outpoint(3)],
      outputs: vec![
        (recipient(), Amount::from_sat(5_000)),
        (change(0), Amount::from_sat(5_000)),
        (change(0), Amount::from_sat(1_774)),
      ],
      target: vec![Target::Postage],
      target_postage: TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      max_postage: TransactionBuilder::DEFAULT_MAX_POSTAGE,
      current_output: 0,
      padding_outputs: 0,
    }
    .build()
    .unwrap();
  }

  #[test]
  fn do_not_select_already_inscribed_sats_for_cardinal_utxos() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(100)),
      (outpoint(2), Amount::from_sat(49 * COIN_VALUE)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::from([(satpoint(2, 10 * COIN_VALUE), inscription_id(1))]),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Err(Error::NotEnoughCardinalUtxos)
    )
  }

  #[test]
  fn do_not_send_two_inscriptions_at_once() {
    let utxos = vec![(outpoint(1), Amount::from_sat(1_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::from([(satpoint(1, 500), inscription_id(1))]),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Err(Error::UtxoContainsAdditionalInscription {
        outgoing_satpoint: satpoint(1, 0),
        inscribed_satpoint: satpoint(1, 500),
        inscription_id: inscription_id(1),
      })
    )
  }

  #[test]
  fn build_transaction_with_custom_fee_rate() {
    let utxos = vec![(outpoint(1), Amount::from_sat(10_000))];

    let fee_rate = FeeRate::try_from(17.3).unwrap();

    let transaction = TransactionBuilder::build_transaction_with_postage(
      satpoint(1, 0),
      BTreeMap::from([(satpoint(1, 0), inscription_id(1))]),
      utxos.into_iter().collect(),
      recipient(),
      alignment(),
      [change(0), change(1)],
      fee_rate,
      None,
      TransactionBuilder::DEFAULT_TARGET_POSTAGE,
      TransactionBuilder::DEFAULT_MAX_POSTAGE,
    )
    .unwrap();

    let fee = fee_rate
      .fee((transaction.weight() + TransactionBuilder::SCHNORR_SIGNATURE_SIZE) as f64 / 4.0 + 1.0);

    pretty_assert_eq!(
      transaction,
      Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(10_000 - fee.to_sat(), recipient())],
      }
    )
  }

  #[test]
  fn exact_transaction_has_correct_value() {
    let utxos = vec![(outpoint(1), Amount::from_sat(5_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(1000, recipient()), tx_out(3870, change(1))],
      })
    )
  }

  #[test]
  fn exact_transaction_adds_output_to_cover_value() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(1_000)),
      (outpoint(2), Amount::from_sat(1_000)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        Amount::from_sat(1500)
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1)), tx_in(outpoint(2))],
        output: vec![tx_out(1500, recipient()), tx_out(312, change(1))],
      })
    )
  }

  #[test]
  fn refuse_to_send_dust() {
    let utxos = vec![(outpoint(1), Amount::from_sat(1_000))];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::from([(satpoint(1, 500), inscription_id(1))]),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        Amount::from_sat(1)
      ),
      Err(Error::Dust {
        output_value: Amount::from_sat(1),
        dust_value: Amount::from_sat(294)
      })
    )
  }

  #[test]
  fn do_not_select_outputs_which_do_not_pay_for_their_own_fee_at_default_fee_rate() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(1_000)),
      (outpoint(2), Amount::from_sat(100)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Err(Error::NotEnoughCardinalUtxos),
    )
  }

  #[test]
  fn do_not_select_outputs_which_do_not_pay_for_their_own_fee_at_higher_fee_rate() {
    let utxos = vec![
      (outpoint(1), Amount::from_sat(1_000)),
      (outpoint(2), Amount::from_sat(500)),
    ];

    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        utxos.into_iter().collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(4.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Err(Error::NotEnoughCardinalUtxos),
    )
  }

  #[test]
  fn additional_input_size_is_correct() {
    let before = TransactionBuilder::estimate_vbytes_with(0, Vec::new());
    let after = TransactionBuilder::estimate_vbytes_with(1, Vec::new());
    assert_eq!(after - before, TransactionBuilder::ADDITIONAL_INPUT_VBYTES);
  }

  #[test]
  fn additional_output_size_is_correct() {
    let before = TransactionBuilder::estimate_vbytes_with(0, Vec::new());
    let after = TransactionBuilder::estimate_vbytes_with(
      0,
      vec![
        "bc1pxwww0ct9ue7e8tdnlmug5m2tamfn7q06sahstg39ys4c9f3340qqxrdu9k"
          .parse()
          .unwrap(),
      ],
    );
    assert_eq!(after - before, TransactionBuilder::ADDITIONAL_OUTPUT_VBYTES);
  }

  #[test]
  fn do_not_strip_excess_value_if_it_would_create_dust() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(1_000))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        Amount::from_sat(707)
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(901, recipient())],
      }),
    );
  }

  #[test]
  fn possible_to_create_output_of_exactly_max_postage() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(20_099))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(1.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(20_000, recipient())],
      }),
    );
  }

  #[test]
  fn do_not_strip_excess_value_if_additional_output_cannot_pay_fee() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(1_500))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(5.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(1005, recipient())],
      }),
    );
  }

  #[test]
  fn correct_error_is_returned_when_fee_cannot_be_paid() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(1_500))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(6.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Err(Error::NotEnoughCardinalUtxos)
    );
  }

  #[test]
  fn recipient_address_must_be_unique() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(1000))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [recipient(), change(1)],
        FeeRate::try_from(0.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Err(Error::DuplicateAddress(recipient()))
    );
  }

  #[test]
  fn change_addresses_must_be_unique() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(1000))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(0)],
        FeeRate::try_from(0.0).unwrap(),
        None,
        Amount::from_sat(1000)
      ),
      Err(Error::DuplicateAddress(change(0)))
    );
  }

  #[test]
  fn output_over_value_because_fees_prevent_excess_value_stripping() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_value(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(2000))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(2.0).unwrap(),
        None,
        Amount::from_sat(1500)
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(1802, recipient())],
      }),
    );
  }

  #[test]
  fn output_over_max_postage_because_fees_prevent_excess_value_stripping() {
    pretty_assert_eq!(
      TransactionBuilder::build_transaction_with_postage(
        satpoint(1, 0),
        BTreeMap::new(),
        vec![(outpoint(1), Amount::from_sat(45000))]
          .into_iter()
          .collect(),
        recipient(),
        alignment(),
        [change(0), change(1)],
        FeeRate::try_from(250.0).unwrap(),
        None,
        TransactionBuilder::DEFAULT_TARGET_POSTAGE,
        TransactionBuilder::DEFAULT_MAX_POSTAGE,
      ),
      Ok(Transaction {
        version: 1,
        lock_time: PackedLockTime::ZERO,
        input: vec![tx_in(outpoint(1))],
        output: vec![tx_out(20250, recipient())],
      }),
    );
  }

  #[test]
  fn select_outgoing_can_select_multiple_utxos() {
    let mut utxos = vec![
      (outpoint(2), Amount::from_sat(3_006)), // 2. biggest utxo is selected 2nd leaving us needing 4206 more
      (outpoint(1), Amount::from_sat(3_003)), // 1. satpoint is selected 1st leaving us needing 7154 more
      (outpoint(5), Amount::from_sat(3_004)),
      (outpoint(4), Amount::from_sat(3_001)), // 4. smallest utxo >= 1259 is selected 4th, filling deficit
      (outpoint(3), Amount::from_sat(3_005)), // 3. next biggest utxo is selected 3rd leaving us needing 1259 more
      (outpoint(6), Amount::from_sat(3_002)),
    ];

    let tx_builder = TransactionBuilder::new(
      satpoint(1, 0),
      BTreeMap::new(),
      utxos.clone().into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Value(Amount::from_sat(10_000))],
      Amount::from_sat(0),
      Amount::from_sat(0),
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .add_value()
    .unwrap();

    utxos.remove(4);
    utxos.remove(3);
    utxos.remove(1);
    utxos.remove(0);
    assert_eq!(
      tx_builder.utxos,
      utxos.iter().map(|(outpoint, _ranges)| *outpoint).collect()
    );
    assert_eq!(
      tx_builder.inputs,
      [outpoint(1), outpoint(2), outpoint(3), outpoint(4)]
    ); // value inputs are pushed at the end
    assert_eq!(
      tx_builder.outputs,
      [(recipient(), Amount::from_sat(3_003 + 3_006 + 3_005 + 3_001))]
    )
  }

  #[test]
  fn pad_alignment_output_can_select_multiple_utxos() {
    let mut utxos = vec![
      (outpoint(4), Amount::from_sat(101)), // 4. smallest utxo >= 84 is selected 4th, filling deficit
      (outpoint(1), Amount::from_sat(20_000)), // 1. satpoint is selected 1st leaving deficit 293
      (outpoint(2), Amount::from_sat(105)), // 2. biggest utxo <= 293 is selected 2nd leaving deficit 188
      (outpoint(5), Amount::from_sat(103)),
      (outpoint(6), Amount::from_sat(10_000)),
      (outpoint(3), Amount::from_sat(104)), // 3. biggest utxo <= 188 is selected 3rd leaving deficit 84
      (outpoint(7), Amount::from_sat(102)),
    ];

    let tx_builder = TransactionBuilder::new(
      satpoint(1, 1),
      BTreeMap::new(),
      utxos.clone().into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Value(Amount::from_sat(10_000))],
      Amount::from_sat(0),
      Amount::from_sat(0),
    )
    .unwrap()
    .select_outgoing()
    .unwrap()
    .align_outgoing()
    .pad_alignment_output()
    .unwrap();

    utxos.remove(5);
    utxos.remove(2);
    utxos.remove(1);
    utxos.remove(0);
    assert_eq!(
      tx_builder.utxos,
      utxos.iter().map(|(outpoint, _ranges)| *outpoint).collect()
    );
    assert_eq!(
      tx_builder.inputs,
      [outpoint(4), outpoint(3), outpoint(2), outpoint(1)]
    ); // padding inputs are inserted at the start
    assert_eq!(
      tx_builder.outputs,
      [
        (change(0), Amount::from_sat(101 + 104 + 105 + 1)),
        (recipient(), Amount::from_sat(19_999))
      ]
    )
  }

  fn select_cardinal_utxo_prefer_under_helper(
    target_value: Amount,
    prefer_under: bool,
    expected_value: Amount,
  ) {
    let utxos = vec![
      (outpoint(4), Amount::from_sat(101)),
      (outpoint(1), Amount::from_sat(20_000)),
      (outpoint(2), Amount::from_sat(105)),
      (outpoint(5), Amount::from_sat(103)),
      (outpoint(6), Amount::from_sat(10_000)),
      (outpoint(3), Amount::from_sat(104)),
      (outpoint(7), Amount::from_sat(102)),
    ];

    let mut tx_builder = TransactionBuilder::new(
      satpoint(0, 0),
      BTreeMap::new(),
      utxos.into_iter().collect(),
      vec![recipient()],
      None,
      [change(0), change(1)],
      FeeRate::try_from(1.0).unwrap(),
      None,
      vec![Target::Value(Amount::from_sat(10_000))],
      Amount::from_sat(0),
      Amount::from_sat(0),
    )
    .unwrap();

    assert_eq!(
      tx_builder
        .select_cardinal_utxo(target_value, prefer_under)
        .unwrap()
        .1,
      expected_value
    );
  }

  #[test]
  fn select_cardinal_utxo_prefer_under() {
    // select biggest utxo <= 104
    select_cardinal_utxo_prefer_under_helper(Amount::from_sat(104), true, Amount::from_sat(104));

    // select biggest utxo <= 1_000
    select_cardinal_utxo_prefer_under_helper(Amount::from_sat(1_000), true, Amount::from_sat(105));

    // select biggest utxo <= 10, else smallest > 10
    select_cardinal_utxo_prefer_under_helper(Amount::from_sat(10), true, Amount::from_sat(101));

    // select smallest utxo >= 104
    select_cardinal_utxo_prefer_under_helper(Amount::from_sat(104), false, Amount::from_sat(104));

    // select smallest utxo >= 1_000
    select_cardinal_utxo_prefer_under_helper(
      Amount::from_sat(1000),
      false,
      Amount::from_sat(10_000),
    );

    // select smallest utxo >= 100_000, else biggest < 100_000
    select_cardinal_utxo_prefer_under_helper(
      Amount::from_sat(100_000),
      false,
      Amount::from_sat(20_000),
    );
  }
}
