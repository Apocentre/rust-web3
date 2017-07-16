use std::time::Duration;
use futures::{IntoFuture, Future, Stream, Poll};
use futures::stream::Skip;
use api::{Eth, EthFilter, Namespace, CreateFilter, FilterStream};
use types::{H256, U256, TransactionRequest, TransactionReceipt};
use helpers::CallResult;
use {Transport, Error};

pub trait ConfirmationCheck {
  type Check: IntoFuture<Item = Option<U256>, Error = Error>;

  fn check(&self) -> Self::Check;
}

enum WaitForConfirmationsState<F, O> {
  WaitForNextBlock,
  CheckConfirmation(F),
  CompareConfirmations(u64, CallResult<U256, O>),
}

struct WaitForConfirmations<T, V, F> where T: Transport {
  transport: T,
  state: WaitForConfirmationsState<F, T::Out>,
  filter_stream: Skip<FilterStream<T, H256>>,
  confirmation_check: V,
  confirmations: u64,
}

impl<T, V, F> Future for WaitForConfirmations<T, V, F::Future> where
  T: Transport,
  V: ConfirmationCheck<Check = F>,
  F: IntoFuture<Item = Option<U256>, Error = Error>,
{

  type Item = ();
  type Error = Error;

  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    loop {
      let next_state = match self.state {
        WaitForConfirmationsState::WaitForNextBlock => {
          let _ = try_ready!(self.filter_stream.poll());
          WaitForConfirmationsState::CheckConfirmation(self.confirmation_check.check().into_future())
        },
        WaitForConfirmationsState::CheckConfirmation(ref mut future) => match try_ready!(future.poll()) {
          Some(confirmation_block_number) => {
            let future = Eth::new(&self.transport).block_number();
            WaitForConfirmationsState::CompareConfirmations(confirmation_block_number.low_u64(), future)
          },
          None => WaitForConfirmationsState::WaitForNextBlock,
        },
        WaitForConfirmationsState::CompareConfirmations(confirmation_block_number, ref mut block_number_future) => {
          let block_number = try_ready!(block_number_future.poll()).low_u64();
          if confirmation_block_number + self.confirmations >= block_number {
            return Ok(().into())
          } else {
            WaitForConfirmationsState::WaitForNextBlock
          }
        },
      };
      self.state = next_state;
    }
  }
}

struct CreateWaitForConfirmations<T: Transport, V> {
  create_filter: CreateFilter<T, H256>,
  poll_interval: Duration,
  transport: Option<T>,
  confirmation_check: Option<V>,
  confirmations: u64,
}

enum ConfirmationsState<T: Transport, V, F> {
  Create(CreateWaitForConfirmations<T, V>),
  Wait(WaitForConfirmations<T, V, F>),
}

pub struct Confirmations<T: Transport, V, F> {
  state: ConfirmationsState<T, V, F>,
}

impl<T: Transport + Clone, V, F> Confirmations<T, V, F> {
  fn new(transport: T, poll_interval: Duration, confirmations: u64, check: V) -> Self {
    let eth = EthFilter::new(transport.clone());
    Confirmations {
      state: ConfirmationsState::Create(CreateWaitForConfirmations {
        create_filter: eth.create_blocks_filter(),
        poll_interval,
        transport: Some(transport),
        confirmation_check: Some(check),
        confirmations,
      })
    }
  }
}

impl<T, V, F> Future for Confirmations<T, V, F::Future> where
  T: Transport,
  V: ConfirmationCheck<Check = F>,
  F: IntoFuture<Item = Option<U256>, Error = Error>,
{

  type Item = ();
  type Error = Error;

  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    loop {
      let next_state = match self.state {
        ConfirmationsState::Create(ref mut create) => {
          let filter = try_ready!(create.create_filter.poll());
          let future = WaitForConfirmations {
            transport: create.transport.take().expect("future polled after ready; qed"),
            state: WaitForConfirmationsState::WaitForNextBlock,
            filter_stream: filter.stream(create.poll_interval).skip(create.confirmations),
            confirmation_check: create.confirmation_check.take().expect("future polled after ready; qed"),
            confirmations: create.confirmations,
          };
          ConfirmationsState::Wait(future)
        },
        ConfirmationsState::Wait(ref mut wait) => return Future::poll(wait),
      };
      self.state = next_state;
    }
  }
}

pub fn wait_for_confirmations<T, V, F>(transport: T, poll_interval: Duration, confirmations: u64, check: V) -> Confirmations<T, V, F::Future> where
  T: Transport + Clone,
  V: ConfirmationCheck<Check = F>,
  F: IntoFuture<Item = Option<U256>, Error = Error>,
{
  Confirmations::new(transport, poll_interval, confirmations, check)
}

struct TransactionReceiptBlockNumber<T: Transport> {
  future: CallResult<Option<TransactionReceipt>, T::Out>,
}

impl<T: Transport> Future for TransactionReceiptBlockNumber<T> {
  type Item = Option<U256>;
  type Error = Error;

  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    let receipt = try_ready!(self.future.poll());
    Ok(receipt.map(|receipt| receipt.block_number).into())
  }
}

struct TransactionReceiptBlockNumberCheck<T: Transport> {
  eth: Eth<T>,
  hash: H256,
}

impl<T: Transport> TransactionReceiptBlockNumberCheck<T> {
  fn new(eth: Eth<T>, hash: H256) -> Self {
    TransactionReceiptBlockNumberCheck {
      eth,
      hash,
    }
  }
}

impl<T: Transport> ConfirmationCheck for TransactionReceiptBlockNumberCheck<T> {
  type Check = TransactionReceiptBlockNumber<T>;

  fn check(&self) -> Self::Check {
    TransactionReceiptBlockNumber {
      future: self.eth.transaction_receipt(self.hash.clone())
    }
  }
}

enum SendTransactionWithConfirmationState<T: Transport> {
  SendTransaction(CallResult<H256, T::Out>),
  WaitForConfirmations(H256, Confirmations<T, TransactionReceiptBlockNumberCheck<T>, TransactionReceiptBlockNumber<T>>),
  GetTransactionReceipt(CallResult<Option<TransactionReceipt>, T::Out>),
}

pub struct SendTransactionWithConfirmation<T: Transport> {
  state: SendTransactionWithConfirmationState<T>,
  eth: Eth<T>,
  transport: T,
  poll_interval: Duration,
  confirmations: u64,
}

impl<T: Transport + Clone> SendTransactionWithConfirmation<T> {
  fn new(transport: T, tx: TransactionRequest, poll_interval: Duration, confirmations: u64) -> Self {
    let eth = Eth::new(transport.clone());
    SendTransactionWithConfirmation {
      state: SendTransactionWithConfirmationState::SendTransaction(eth.send_transaction(tx)),
      eth,
      transport,
      poll_interval,
      confirmations,
    }
  }
}

impl<T: Transport + Clone> Future for SendTransactionWithConfirmation<T> {
  type Item = TransactionReceipt;
  type Error = Error;

  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    loop {
      let next_state = match self.state {
        SendTransactionWithConfirmationState::SendTransaction(ref mut future) => {
          let hash = try_ready!(future.poll());
          let confirmation_check = TransactionReceiptBlockNumberCheck::new(Eth::new(self.transport.clone()), hash.clone());
          let wait = wait_for_confirmations(self.transport.clone(), self.poll_interval, self.confirmations, confirmation_check);
          SendTransactionWithConfirmationState::WaitForConfirmations(hash, wait)
        },
        SendTransactionWithConfirmationState::WaitForConfirmations(hash, ref mut future) => {
          let _confirmed = try_ready!(Future::poll(future));
          let receipt_future = self.eth.transaction_receipt(hash);
          SendTransactionWithConfirmationState::GetTransactionReceipt(receipt_future)
        },
        SendTransactionWithConfirmationState::GetTransactionReceipt(ref mut future) => {
          let receipt = try_ready!(Future::poll(future)).expect("receipt can't be null after wait for confirmations; qed");
          return Ok(receipt.into());
        },
      };
      self.state = next_state;
    }
  }
}

pub fn send_transaction_with_confirmation<T>(transport: T, tx: TransactionRequest, poll_interval: Duration, confirmations: u64) -> SendTransactionWithConfirmation<T> where T: Transport + Clone {
  SendTransactionWithConfirmation::new(transport, tx, poll_interval, confirmations)
}

#[cfg(test)]
mod tests {
  use std::time::Duration;
  use futures::Future;
  use helpers::tests::TestTransport;
  use types::{TransactionRequest, TransactionReceipt};
  use super::send_transaction_with_confirmation;
  use rpc::Value;

  #[test]
  fn test_send_transaction_with_confirmation() {
    let mut transport = TestTransport::default();
    let confirmations = 3;
    let transaction_request = TransactionRequest {
      from: 0x123.into(),
      to: Some(0x123.into()),
      gas: None,
      gas_price: Some(1.into()),
      value: Some(1.into()),
      data: None,
      nonce: None,
      condition: None,
    };
    let transaction_receipt = TransactionReceipt {
      hash: 0.into(),
      index: 0.into(),
      block_hash: 0.into(),
      block_number: 2.into(),
      cumulative_gas_used: 0.into(),
      gas_used: 0.into(),
      contract_address: None,
      logs: vec![],
    };

    let poll_interval = Duration::from_secs(0);
    transport.add_response(Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000111"#.into()));
    transport.add_response(Value::String("0x123".into()));
    transport.add_response(Value::Array(vec![
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000456"#.into()),
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000457"#.into()),
    ]));
    transport.add_response(Value::Array(vec![
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000458"#.into()),
    ]));
    transport.add_response(Value::Array(vec![
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000459"#.into()),
    ]));
    transport.add_response(Value::Null);
    transport.add_response(Value::Array(vec![
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000460"#.into()),
      Value::String(r#"0x0000000000000000000000000000000000000000000000000000000000000461"#.into()),
    ]));
    transport.add_response(Value::Null);
    transport.add_response(json!(transaction_receipt));
    transport.add_response(Value::String("0x5".into()));
    transport.add_response(json!(transaction_receipt));
    transport.add_response(Value::Bool(true));

    let confirmation = {
      let future = send_transaction_with_confirmation(&transport, transaction_request, poll_interval, confirmations);
      future.wait()
    };

    transport.assert_request("eth_sendTransaction", &[r#"{"from":"0x0000000000000000000000000000000000000123","gasPrice":"0x1","to":"0x0000000000000000000000000000000000000123","value":"0x1"}"#.into()]);
    transport.assert_request("eth_newBlockFilter", &[]);
    transport.assert_request("eth_getFilterChanges", &[r#""0x123""#.into()]);
    transport.assert_request("eth_getFilterChanges", &[r#""0x123""#.into()]);
    transport.assert_request("eth_getFilterChanges", &[r#""0x123""#.into()]);
    transport.assert_request("eth_getTransactionReceipt", &[r#""0x0000000000000000000000000000000000000000000000000000000000000111""#.into()]);
    transport.assert_request("eth_getFilterChanges", &[r#""0x123""#.into()]);
    transport.assert_request("eth_getTransactionReceipt", &[r#""0x0000000000000000000000000000000000000000000000000000000000000111""#.into()]);
    transport.assert_request("eth_getTransactionReceipt", &[r#""0x0000000000000000000000000000000000000000000000000000000000000111""#.into()]);
    transport.assert_request("eth_blockNumber", &[]);
    transport.assert_request("eth_getTransactionReceipt", &[r#""0x0000000000000000000000000000000000000000000000000000000000000111""#.into()]);
    transport.assert_request("eth_uninstallFilter", &[r#""0x123""#.into()]);
    transport.assert_no_more_requests();
    assert_eq!(confirmation, Ok(transaction_receipt));
  }
}