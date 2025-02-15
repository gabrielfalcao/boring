use boring::ex_data::Index;
use boring::ssl::{self, ClientHello, PrivateKeyMethod, Ssl, SslContextBuilder};
use once_cell::sync::Lazy;
use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll, Waker};

/// The type of futures to pass to [`SslContextBuilderExt::set_async_select_certificate_callback`].
pub type BoxSelectCertFuture = ExDataFuture<Result<BoxSelectCertFinish, AsyncSelectCertError>>;

/// The type of callbacks returned by [`BoxSelectCertFuture`] methods.
pub type BoxSelectCertFinish = Box<dyn FnOnce(ClientHello<'_>) -> Result<(), AsyncSelectCertError>>;

/// The type of futures returned by [`AsyncPrivateKeyMethod`] methods.
pub type BoxPrivateKeyMethodFuture =
    ExDataFuture<Result<BoxPrivateKeyMethodFinish, AsyncPrivateKeyMethodError>>;

/// The type of callbacks returned by [`BoxPrivateKeyMethodFuture`].
pub type BoxPrivateKeyMethodFinish =
    Box<dyn FnOnce(&mut ssl::SslRef, &mut [u8]) -> Result<usize, AsyncPrivateKeyMethodError>>;

/// Convenience alias for futures stored in [`Ssl`] ex data by [`SslContextBuilderExt`] methods.
///
/// Public for documentation purposes.
pub type ExDataFuture<T> = Pin<Box<dyn Future<Output = T> + Send + Sync>>;

pub(crate) static TASK_WAKER_INDEX: Lazy<Index<Ssl, Option<Waker>>> =
    Lazy::new(|| Ssl::new_ex_index().unwrap());
pub(crate) static SELECT_CERT_FUTURE_INDEX: Lazy<Index<Ssl, Option<BoxSelectCertFuture>>> =
    Lazy::new(|| Ssl::new_ex_index().unwrap());
pub(crate) static SELECT_PRIVATE_KEY_METHOD_FUTURE_INDEX: Lazy<
    Index<Ssl, Option<BoxPrivateKeyMethodFuture>>,
> = Lazy::new(|| Ssl::new_ex_index().unwrap());

/// Extensions to [`SslContextBuilder`].
///
/// This trait provides additional methods to use async callbacks with boring.
pub trait SslContextBuilderExt: private::Sealed {
    /// Sets a callback that is called before most [`ClientHello`] processing
    /// and before the decision whether to resume a session is made. The
    /// callback may inspect the [`ClientHello`] and configure the connection.
    ///
    /// This method uses a function that returns a future whose output is
    /// itself a closure that will be passed [`ClientHello`] to configure
    /// the connection based on the computations done in the future.
    ///
    /// See [`SslContextBuilder::set_select_certificate_callback`] for the sync
    /// setter of this callback.
    fn set_async_select_certificate_callback<F>(&mut self, callback: F)
    where
        F: Fn(&mut ClientHello<'_>) -> Result<BoxSelectCertFuture, AsyncSelectCertError>
            + Send
            + Sync
            + 'static;

    /// Configures a custom private key method on the context.
    ///
    /// See [`AsyncPrivateKeyMethod`] for more details.
    fn set_async_private_key_method(&mut self, method: impl AsyncPrivateKeyMethod);
}

impl SslContextBuilderExt for SslContextBuilder {
    fn set_async_select_certificate_callback<F>(&mut self, callback: F)
    where
        F: Fn(&mut ClientHello<'_>) -> Result<BoxSelectCertFuture, AsyncSelectCertError>
            + Send
            + Sync
            + 'static,
    {
        self.set_select_certificate_callback(move |mut client_hello| {
            let fut_poll_result = with_ex_data_future(
                &mut client_hello,
                *SELECT_CERT_FUTURE_INDEX,
                ClientHello::ssl_mut,
                &callback,
            );

            let fut_result = match fut_poll_result {
                Poll::Ready(fut_result) => fut_result,
                Poll::Pending => return Err(ssl::SelectCertError::RETRY),
            };

            let finish = fut_result.or(Err(ssl::SelectCertError::ERROR))?;

            finish(client_hello).or(Err(ssl::SelectCertError::ERROR))
        })
    }

    fn set_async_private_key_method(&mut self, method: impl AsyncPrivateKeyMethod) {
        self.set_private_key_method(AsyncPrivateKeyMethodBridge(Box::new(method)));
    }
}

/// A fatal error to be returned from async select certificate callbacks.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AsyncSelectCertError;

/// Describes async private key hooks. This is used to off-load signing
/// operations to a custom, potentially asynchronous, backend. Metadata about the
/// key such as the type and size are parsed out of the certificate.
///
/// See [`PrivateKeyMethod`] for the sync version of those hooks.
///
/// [`ssl_private_key_method_st`]: https://commondatastorage.googleapis.com/chromium-boringssl-docs/ssl.h.html#ssl_private_key_method_st
pub trait AsyncPrivateKeyMethod: Send + Sync + 'static {
    /// Signs the message `input` using the specified signature algorithm.
    ///
    /// This method uses a function that returns a future whose output is
    /// itself a closure that will be passed `ssl` and `output`
    /// to finish writing the signature.
    ///
    /// See [`PrivateKeyMethod::sign`] for the sync version of this method.
    fn sign(
        &self,
        ssl: &mut ssl::SslRef,
        input: &[u8],
        signature_algorithm: ssl::SslSignatureAlgorithm,
        output: &mut [u8],
    ) -> Result<BoxPrivateKeyMethodFuture, AsyncPrivateKeyMethodError>;

    /// Decrypts `input`.
    ///
    /// This method uses a function that returns a future whose output is
    /// itself a closure that will be passed `ssl` and `output`
    /// to finish decrypting the input.
    ///
    /// See [`PrivateKeyMethod::decrypt`] for the sync version of this method.
    fn decrypt(
        &self,
        ssl: &mut ssl::SslRef,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<BoxPrivateKeyMethodFuture, AsyncPrivateKeyMethodError>;
}

/// A fatal error to be returned from async private key methods.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AsyncPrivateKeyMethodError;

struct AsyncPrivateKeyMethodBridge(Box<dyn AsyncPrivateKeyMethod>);

impl PrivateKeyMethod for AsyncPrivateKeyMethodBridge {
    fn sign(
        &self,
        ssl: &mut ssl::SslRef,
        input: &[u8],
        signature_algorithm: ssl::SslSignatureAlgorithm,
        output: &mut [u8],
    ) -> Result<usize, ssl::PrivateKeyMethodError> {
        with_private_key_method(ssl, output, |ssl, output| {
            <dyn AsyncPrivateKeyMethod>::sign(&*self.0, ssl, input, signature_algorithm, output)
        })
    }

    fn decrypt(
        &self,
        ssl: &mut ssl::SslRef,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ssl::PrivateKeyMethodError> {
        with_private_key_method(ssl, output, |ssl, output| {
            <dyn AsyncPrivateKeyMethod>::decrypt(&*self.0, ssl, input, output)
        })
    }

    fn complete(
        &self,
        ssl: &mut ssl::SslRef,
        output: &mut [u8],
    ) -> Result<usize, ssl::PrivateKeyMethodError> {
        with_private_key_method(ssl, output, |_, _| {
            // This should never be reached, if it does, that's a bug on boring's side,
            // which called `complete` without having been returned to with a pending
            // future from `sign` or `decrypt`.

            if cfg!(debug_assertions) {
                panic!("BUG: boring called complete without a pending operation");
            }

            Err(AsyncPrivateKeyMethodError)
        })
    }
}

/// Creates and drives a private key method future.
///
/// This is a convenience function for the three methods of impl `PrivateKeyMethod``
/// for `dyn AsyncPrivateKeyMethod`. It relies on [`with_ex_data_future`] to
/// drive the future and then immediately calls the final [`BoxPrivateKeyMethodFinish`]
/// when the future is ready.
fn with_private_key_method(
    ssl: &mut ssl::SslRef,
    output: &mut [u8],
    create_fut: impl FnOnce(
        &mut ssl::SslRef,
        &mut [u8],
    ) -> Result<BoxPrivateKeyMethodFuture, AsyncPrivateKeyMethodError>,
) -> Result<usize, ssl::PrivateKeyMethodError> {
    let fut_poll_result = with_ex_data_future(
        ssl,
        *SELECT_PRIVATE_KEY_METHOD_FUTURE_INDEX,
        |ssl| ssl,
        |ssl| create_fut(ssl, output),
    );

    let fut_result = match fut_poll_result {
        Poll::Ready(fut_result) => fut_result,
        Poll::Pending => return Err(ssl::PrivateKeyMethodError::RETRY),
    };

    let finish = fut_result.or(Err(ssl::PrivateKeyMethodError::FAILURE))?;

    finish(ssl, output).or(Err(ssl::PrivateKeyMethodError::FAILURE))
}

/// Creates and drives a future stored in `ssl_handle`'s `Ssl` at ex data index `index`.
///
/// This function won't even bother storing the future in `index` if the future
/// created by `create_fut` returns `Poll::Ready(_)` on the first poll call.
fn with_ex_data_future<H, T, E>(
    ssl_handle: &mut H,
    index: Index<ssl::Ssl, Option<ExDataFuture<Result<T, E>>>>,
    get_ssl_mut: impl Fn(&mut H) -> &mut ssl::SslRef,
    create_fut: impl FnOnce(&mut H) -> Result<ExDataFuture<Result<T, E>>, E>,
) -> Poll<Result<T, E>> {
    let ssl = get_ssl_mut(ssl_handle);
    let waker = ssl
        .ex_data(*TASK_WAKER_INDEX)
        .cloned()
        .flatten()
        .expect("task waker should be set");

    let mut ctx = Context::from_waker(&waker);

    if let Some(data @ Some(_)) = ssl.ex_data_mut(index) {
        let fut_result = ready!(data.as_mut().unwrap().as_mut().poll(&mut ctx));

        *data = None;

        Poll::Ready(fut_result)
    } else {
        let mut fut = create_fut(ssl_handle)?;

        match fut.as_mut().poll(&mut ctx) {
            Poll::Ready(fut_result) => Poll::Ready(fut_result),
            Poll::Pending => {
                get_ssl_mut(ssl_handle).set_ex_data(index, Some(fut));

                Poll::Pending
            }
        }
    }
}

mod private {
    pub trait Sealed {}
}

impl private::Sealed for SslContextBuilder {}
