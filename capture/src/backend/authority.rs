//! Validated authority connections and their independent retirement verifier.
use crate::retirement::Retirement;
use amaterasu_vkms_frame_helper::{
    authority::{RetirementReceipt, RetiringHelperClient},
    client::HelperClient,
};
use std::{io, os::fd::BorrowedFd, sync::Arc};

pub(super) trait ApprovedSession: Send {
    fn client(&mut self) -> &mut HelperClient;
    fn retirement(&self) -> Arc<dyn Retirement>;
}

impl ApprovedSession for RetiringHelperClient {
    fn client(&mut self) -> &mut HelperClient {
        Self::client(self)
    }
    fn retirement(&self) -> Arc<dyn Retirement> {
        self.retirement_receipt()
    }
}

impl Retirement for RetirementReceipt {
    fn poll_retired(&self) -> io::Result<bool> {
        Self::poll_retired(self)
    }
}

pub(super) trait AuthorityConnector: Send + Sync {
    fn connect(&self, control: BorrowedFd<'_>) -> io::Result<Box<dyn ApprovedSession>>;
}

pub(super) struct NativeAuthority;
impl AuthorityConnector for NativeAuthority {
    fn connect(&self, control: BorrowedFd<'_>) -> io::Result<Box<dyn ApprovedSession>> {
        amaterasu_vkms_frame_helper::authority::connect_authorized_retiring(control)
            .map(|client| Box::new(client) as Box<dyn ApprovedSession>)
    }
}
