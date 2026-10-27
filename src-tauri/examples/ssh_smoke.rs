fn main() {
    if std::env::args().nth(1).as_deref() == Some("--keyring") {
        sync_ui_lib::integration::keyring().expect("keyring integration failed");
    } else {
        sync_ui_lib::integration::run().expect("SSH integration failed");
    }
}
