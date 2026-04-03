pub mod cli;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::GatewayError;
use crate::messages::cli_output::CliOutputEvent;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&mut self) -> Result<(), GatewayError>;
    async fn write(&self, data: &str) -> Result<(), GatewayError>;
    async fn close(&mut self) -> Result<(), GatewayError>;
    fn is_ready(&self) -> bool;
    fn session_id(&self) -> Option<&str>;
    fn event_receiver(&mut self) -> Option<mpsc::Receiver<Result<CliOutputEvent, GatewayError>>>;
}
