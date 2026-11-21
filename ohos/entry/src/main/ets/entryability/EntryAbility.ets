import { NativeAbility } from "@ohos-rs/ability";

// The whole app is the Rust library libmalasdoh_ohos.so (crates/malasdoh-ohos):
// NativeAbility loads it, gives it a surface to draw on and passes it the
// app's lifecycle and input.
export default class EntryAbility extends NativeAbility {
  public moduleName: string = "malasdoh_ohos";
}
