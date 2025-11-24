use crate::api::PowerInfo;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;

pub trait Notifier: Send + Sync {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>>;
}

pub struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            println!(
                "⚠️ [Low Power Warning] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                info.room_display_name, info.remaining_money, info.remaining_energy
            );
            Ok(())
        })
    }
}
