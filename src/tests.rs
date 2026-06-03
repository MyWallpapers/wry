use super::*;

#[test]
#[cfg_attr(miri, ignore)]
fn should_get_webview_version() {
  if let Err(error) = webview_version() {
    panic!("{}", error);
  }
}
