# Mac RDP Secure Access Icons · V2

This version centres on secure, authorised, cross-device access to the Mac desktop.

## Visual meaning
- Large window: the macOS host desktop; the three coloured window controls hint at the platform.
- Small window: a generic RDP client, with a neutral window mark.
- Shield and closed padlock: placed inside the Mac desktop, they stand for protected, authorised access.
- Complete window outlines: the shapes stay stable and closed. Blue and silver-white continue the original visual family; mint green carries the idea of security.
- Menu bar: simplified separately for small sizes to a closed padlock inside a complete window; the state is added outside the window, and every outline stays complete.

## Contents
- app/app-light.png, app/app-dark.png: 1254 × 1254 PNG master artwork, generated with the built-in ImageGen.
- menu/: 5 monochrome SVG templates on a 24 × 20 canvas; black + transparent.
- RDPHost.xcassets/: an Xcode asset catalog of the same menu bar SVGs, marked for template rendering and preserved vector data.
- preview.html: an offline preview with the light and dark icons, the menu bar icons and the icons scaled down.
- prompts.md: the final generation and editing prompts for this version.

| Asset | Meaning | Added shape |
| --- | --- | --- |
| HostTemplate | Secure remote desktop brand mark | None |
| ReadyTemplate | Service on, waiting for connections | Hollow circle |
| ActiveTemplate | An active remote session exists | Filled circle |
| PausedTemplate | Service paused | Two vertical bars |
| ErrorTemplate | Service error, needs attention | Exclamation mark |

## Integration notes
The suggested starting size for the menu bar assets is 24 × 20 pt; check the light, dark, selected and high-contrast appearances in a real macOS menu bar. Give each state matching menu text and an accessibility label. The size is this design's own value, not one Apple requires.

The app PNGs are master artwork; no dynamic layered .icon or legacy .icns has been made yet. A production version can rebuild the main desktop, the client, the shield and the padlock as separate vector layers, set up materials and appearances in Icon Composer, and let the tool apply the system mask. The CSS rounded corners on the preview page are only an approximation.

References:
- [Apple App icons](https://developer.apple.com/design/human-interface-guidelines/app-icons)
- [Icon Composer](https://developer.apple.com/documentation/xcode/creating-your-app-icon-using-icon-composer)
- [NSImage.isTemplate](https://developer.apple.com/documentation/appkit/nsimage/istemplate)

## Checks done
The main app images were checked by eye; the PNG sizes, the SVG/XML, the asset catalog JSON, the local preview references and the integrity of the archive were checked. This time there was no Xcode build, and no screenshot check in a real menu bar or of the preview page in a browser.
