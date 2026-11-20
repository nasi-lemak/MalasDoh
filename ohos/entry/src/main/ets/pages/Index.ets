// NativeAbility shows its own page with the native surface; this one is only
// here because a HarmonyOS app has to list at least one page.
@Entry
@Component
struct Index {
  build() {
    Column() {
      Text("MalasDoh")
    }
    .width("100%")
    .height("100%")
    .justifyContent(FlexAlign.Center)
  }
}
