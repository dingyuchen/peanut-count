# Peanut Count for iOS

The iOS app is built with React Native and Expo. It receives PDFs through
the iOS Share Sheet and uses PDFKit to extract selectable text on-device.
The source PDF is never sent to the sync server.

## Build

Install Xcode, CocoaPods, and Node.js. From this directory:

```sh
npm ci
npx expo prebuild --platform ios
npm run ios
```

The app and its Share Extension require a native build. For a physical
device, assign both targets to your Apple development team and register
the App Group `group.org.peanutcount.ios`. Generated native files are
ignored. Change `app.json` to configure the app and the Share Extension.
Expo's share-to-app handoff is experimental on iOS. Use the in-app file
picker if the Share Sheet does not open Peanut Count.
[Expo Sharing documentation](https://docs.expo.dev/versions/v56.0.0/sdk/sharing/).

## Import a statement

1. Sign up or sign in to a self-hosted sync server, or choose **Continue without sync**.
2. Share a PDF from Files or another app to **Peanut Count**. You can also choose a PDF from inside the app.
3. Enter the account shown on the statement, its currency, and the date order.
4. Review the imported rows. Edit the proposed counter-accounts or exclude rows, then choose **Add transactions**.

The parser handles single-line text rows with a full date, description, and
a two-decimal amount. Amounts need an explicit sign, parentheses for
negative values, or a `CR`/`DR` marker. A `Date … Amount Balance` header
also allows a trailing running balance. Use the signed change to your
asset or liability account when checking a preview. The parser reports
dated lines it could not read. Scans, multiline transactions, and separate
debit and credit columns are outside the supported formats.

Imports use exact decimal strings and add equal and opposite postings to
the statement account and the reviewed counter-account. The file fingerprint
prevents reimporting the same PDF into one ledger. Overlapping date ranges
across different files still need review.

## Sync

See the [server setup guide](../../README.md#mobile-accounts-and-statement-sync).
An account-backed import is written locally first and queued for upload if
the server is unreachable. Use **Sync now** to retry or pull from other
devices. The device-only ledger stays separate from account-backed ledgers.
Signing up does not upload existing local-only imports.

`npm test` runs the parser fixtures.
