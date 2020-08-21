// Copyright 2017-2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Utilities for testing subsystems.

use polkadot_node_subsystem::messages::AllMessages;
use polkadot_node_subsystem::{FromOverseer, SubsystemContext, SubsystemError, SubsystemResult};

use futures::channel::mpsc;
use futures::poll;
use futures::prelude::*;
use futures_timer::Delay;
use parking_lot::Mutex;
use pin_project::pin_project;
use sp_core::{testing::TaskExecutor, traits::SpawnNamed};

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

enum SinkState<T> {
	Empty {
		read_waker: Option<Waker>,
	},
	Item {
		item: T,
		ready_waker: Option<Waker>,
		flush_waker: Option<Waker>,
	},
}

/// The sink half of a single-item sink that does not resolve until the item has been read.
pub struct SingleItemSink<T>(Arc<Mutex<SinkState<T>>>);

/// The stream half of a single-item sink.
pub struct SingleItemStream<T>(Arc<Mutex<SinkState<T>>>);

impl<T> Sink<T> for SingleItemSink<T> {
	type Error = Infallible;

	fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Infallible>> {
		let mut state = self.0.lock();
		match *state {
			SinkState::Empty { .. } => Poll::Ready(Ok(())),
			SinkState::Item {
				ref mut ready_waker,
				..
			} => {
				*ready_waker = Some(cx.waker().clone());
				Poll::Pending
			}
		}
	}

	fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Infallible> {
		let mut state = self.0.lock();

		match *state {
			SinkState::Empty { ref mut read_waker } => {
				if let Some(waker) = read_waker.take() {
					waker.wake();
				}
			}
			_ => panic!("start_send called outside of empty sink state ensured by poll_ready"),
		}

		*state = SinkState::Item {
			item,
			ready_waker: None,
			flush_waker: None,
		};

		Ok(())
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Infallible>> {
		let mut state = self.0.lock();
		match *state {
			SinkState::Empty { .. } => Poll::Ready(Ok(())),
			SinkState::Item {
				ref mut flush_waker,
				..
			} => {
				*flush_waker = Some(cx.waker().clone());
				Poll::Pending
			}
		}
	}

	fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Infallible>> {
		self.poll_flush(cx)
	}
}

impl<T> Stream for SingleItemStream<T> {
	type Item = T;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		let mut state = self.0.lock();

		let read_waker = Some(cx.waker().clone());

		match std::mem::replace(&mut *state, SinkState::Empty { read_waker }) {
			SinkState::Empty { .. } => Poll::Pending,
			SinkState::Item {
				item,
				ready_waker,
				flush_waker,
			} => {
				if let Some(waker) = ready_waker {
					waker.wake();
				}

				if let Some(waker) = flush_waker {
					waker.wake();
				}

				Poll::Ready(Some(item))
			}
		}
	}
}

/// Create a single-item Sink/Stream pair.
///
/// The sink's send methods resolve at the point which the stream reads the item,
/// not when the item is buffered.
pub fn single_item_sink<T>() -> (SingleItemSink<T>, SingleItemStream<T>) {
	let inner = Arc::new(Mutex::new(SinkState::Empty { read_waker: None }));
	(SingleItemSink(inner.clone()), SingleItemStream(inner))
}

/// A test subsystem context.
pub struct TestSubsystemContext<M, S> {
	tx: mpsc::UnboundedSender<AllMessages>,
	rx: SingleItemStream<FromOverseer<M>>,
	spawn: S,
}

#[async_trait::async_trait]
impl<M: Send + 'static, S: SpawnNamed + Send + 'static> SubsystemContext
	for TestSubsystemContext<M, S>
{
	type Message = M;

	async fn try_recv(&mut self) -> Result<Option<FromOverseer<M>>, ()> {
		match poll!(self.rx.next()) {
			Poll::Ready(Some(msg)) => Ok(Some(msg)),
			Poll::Ready(None) => Err(()),
			Poll::Pending => Ok(None),
		}
	}

	async fn recv(&mut self) -> SubsystemResult<FromOverseer<M>> {
		self.rx.next().await.ok_or(SubsystemError)
	}

	async fn spawn(
		&mut self,
		name: &'static str,
		s: Pin<Box<dyn Future<Output = ()> + Send>>,
	) -> SubsystemResult<()> {
		self.spawn.spawn(name, s);
		Ok(())
	}

	async fn spawn_blocking(&mut self, name: &'static str, s: Pin<Box<dyn Future<Output = ()> + Send>>)
		-> SubsystemResult<()>
	{
		self.spawn.spawn_blocking(name, s);
		Ok(())
	}

	async fn send_message(&mut self, msg: AllMessages) -> SubsystemResult<()> {
		self.tx
			.send(msg)
			.await
			.expect("test overseer no longer live");
		Ok(())
	}

	async fn send_messages<T>(&mut self, msgs: T) -> SubsystemResult<()>
	where
		T: IntoIterator<Item = AllMessages> + Send,
		T::IntoIter: Send,
	{
		let mut iter = stream::iter(msgs.into_iter().map(Ok));
		self.tx
			.send_all(&mut iter)
			.await
			.expect("test overseer no longer live");

		Ok(())
	}
}

/// A handle for interacting with the subsystem context.
pub struct TestSubsystemContextHandle<M> {
	tx: SingleItemSink<FromOverseer<M>>,
	rx: mpsc::UnboundedReceiver<AllMessages>,
}

impl<M> TestSubsystemContextHandle<M> {
	/// Send a message or signal to the subsystem. This resolves at the point in time where the
	/// subsystem has _read_ the message.
	pub async fn send(&mut self, from_overseer: FromOverseer<M>) {
		self.tx
			.send(from_overseer)
			.await
			.expect("Test subsystem no longer live");
	}

	/// Receive the next message from the subsystem.
	pub async fn recv(&mut self) -> AllMessages {
		self.try_recv().await.expect("Test subsystem no longer live")
	}

	/// Receive the next message from the subsystem, or `None` if the channel has been closed.
	pub async fn try_recv(&mut self) -> Option<AllMessages> {
		self.rx.next().await
	}
}

/// Make a test subsystem context.
pub fn make_subsystem_context<M, S>(
	spawn: S,
) -> (TestSubsystemContext<M, S>, TestSubsystemContextHandle<M>) {
	let (overseer_tx, overseer_rx) = single_item_sink();
	let (all_messages_tx, all_messages_rx) = mpsc::unbounded();

	(
		TestSubsystemContext {
			tx: all_messages_tx,
			rx: overseer_rx,
			spawn,
		},
		TestSubsystemContextHandle {
			tx: overseer_tx,
			rx: all_messages_rx,
		},
	)
}

/// Test a subsystem, mocking the overseer
///
/// Pass in two async closures: one mocks the overseer, the other runs the test from the perspective of a subsystem.
///
/// Times out in two seconds.
pub fn subsystem_test_harness<M, OverseerFactory, Overseer, TestFactory, Test>(
	overseer_factory: OverseerFactory,
	test_factory: TestFactory,
) where
	OverseerFactory: FnOnce(TestSubsystemContextHandle<M>) -> Overseer,
	Overseer: Future<Output = ()>,
	TestFactory: FnOnce(TestSubsystemContext<M, TaskExecutor>) -> Test,
	Test: Future<Output = ()>,
{
	let pool = TaskExecutor::new();
	let (context, handle) = make_subsystem_context(pool);
	let overseer = overseer_factory(handle);
	let test = test_factory(context);

	let timeout = Delay::new(Duration::from_secs(2));

	futures::pin_mut!(overseer, test, timeout);

	futures::executor::block_on(async move {
		futures::select! {
			_ = overseer.fuse() => (),
			_ = test.fuse() => (),
			_ = timeout.fuse() => panic!("test timed out instead of completing"),
		}
	});
}

/// A future that wraps another future with a `Delay` allowing for time-limited futures.
#[pin_project]
pub struct Timeout<F: Future> {
	#[pin]
	future: F,
	#[pin]
	delay: Delay,
}

/// Extends `Future` to allow time-limited futures.
pub trait TimeoutExt: Future {
	fn timeout(self, duration: Duration) -> Timeout<Self>
	where
		Self: Sized,
	{
		Timeout {
			future: self,
			delay: Delay::new(duration),
		}
	}
}

impl<F: Future> TimeoutExt for F {}

impl<F: Future> Future for Timeout<F> {
	type Output = Option<F::Output>;

	fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
		let this = self.project();

		if this.delay.poll(ctx).is_ready() {
			return Poll::Ready(None);
		}

		if let Poll::Ready(output) = this.future.poll(ctx) {
			return Poll::Ready(Some(output));
		}

		Poll::Pending
	}
}