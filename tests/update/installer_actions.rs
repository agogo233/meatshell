// These deferred MSI actions interrupt an actual installation transaction.
// Windows Installer must roll back the file replacement and previous-product removal.
#[no_mangle]
pub extern "system" fn CancelInstall(_session: u32) -> u32 {
    1602 // ERROR_INSTALL_USEREXIT
}

#[no_mangle]
pub extern "system" fn FailInstall(_session: u32) -> u32 {
    1603 // ERROR_INSTALL_FAILURE
}
