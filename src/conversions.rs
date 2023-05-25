use anyhow::Result;
use async_trait::async_trait;

use crate::Client;

#[async_trait]
pub trait TryFromAsync<T>: Sized + Send {
    async fn try_from_async(value: T, client: Client) -> Result<Self>;
}

#[async_trait]
pub trait TryIntoAsync<T>: Sized + Send {
    async fn try_into_async(self, client: Client) -> Result<T>;
}

#[async_trait]
impl<T, U> TryIntoAsync<U> for T
where
    U: TryFromAsync<T>,
    T: Send,
{
    async fn try_into_async(self, client: Client) -> Result<U> {
        U::try_from_async(self, client).await
    }
}
