import XCTest

// Drives Peanut Count through the first-run flow and asks the capture server
// (scripts/capture.py, port 18745) to start/stop one recording per clip and to
// save numbered stills. Each clip is a func so a failed run can resume from it.
final class Flow: XCTestCase {
    let app = XCUIApplication(bundleIdentifier: "org.peanutcount.ios")

    // MARK: capture server

    func capture(_ name: String) {
        pause(name == "stop" ? 1 : 0.65)
        let finished = expectation(description: name)
        URLSession.shared.dataTask(with: URL(string: "http://127.0.0.1:18745/" + name)!) { _, response, error in
            XCTAssertNil(error)
            XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200)
            finished.fulfill()
        }.resume()
        wait(for: [finished], timeout: 15)
    }

    // MARK: helpers

    func pause(_ seconds: Double = 0.65) { Thread.sleep(forTimeInterval: seconds) }

    func item(_ label: String) -> XCUIElement {
        app.descendants(matching: .any).matching(NSPredicate(format: "label == %@", label)).firstMatch
    }

    func file(_ prefix: String) -> XCUIElement {
        app.descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH[c] %@", prefix)).firstMatch
    }

    func tap(_ label: String) throws {
        let element = item(label)
        guard element.waitForExistence(timeout: 8) else {
            print(app.debugDescription)
            throw NSError(domain: "Missing " + label, code: 1)
        }
        element.tap()
        pause()
    }

    func drag(_ from: Double = 0.82, _ to: Double = 0.18) {
        let start = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: from))
        let end = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: to))
        start.press(forDuration: 0.1, thenDragTo: end, withVelocity: .slow, thenHoldForDuration: 0.05)
    }

    // Types one key at a time so the recording shows the keyboard in use.
    func type(_ text: String, into placeholder: String, still: String? = nil) {
        let field = app.textFields.matching(NSPredicate(format: "placeholderValue == %@", placeholder)).firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.tap()
        pause()
        // An empty field reports its placeholder; Find is enabled only once both fields hold
        // text, so a value equal to the placeholder with Find enabled is a prefilled default.
        if let old = field.value as? String, old != placeholder || app.buttons["Find transactions"].isEnabled {
            field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: old.count))
        }
        for (index, key) in text.enumerated() {
            field.typeText(String(key))
            if index == 2, let still { capture(still) }
        }
        XCTAssertEqual(field.value as? String, text)
    }

    func review(_ count: Int) {
        let find = app.buttons["Find transactions"]
        XCTAssertTrue(find.isEnabled)
        find.tap()
        XCTAssertTrue(item("\(count) transactions found").waitForExistence(timeout: 15))
    }

    // Opens On My iPhone in the document picker and waits for the fixture.
    func pick(_ prefix: String) throws {
        let picker = app.descendants(matching: .any).matching(NSPredicate(
            format: "label IN %@ OR label BEGINSWITH[c] %@", ["Browse", "On My iPhone"], prefix)).firstMatch
        XCTAssertTrue(picker.waitForExistence(timeout: 35))
        if !file(prefix).exists {
            if app.buttons["Browse"].exists && !app.buttons["Browse"].isSelected { try tap("Browse") }
            if !item("On My iPhone").exists && app.buttons["BackButton"].exists { app.buttons["BackButton"].tap() }
            if item("On My iPhone").exists { try tap("On My iPhone") }
        }
        guard file(prefix).waitForExistence(timeout: 8) else {
            print(app.debugDescription)
            throw NSError(domain: "Missing PDF " + prefix, code: 2)
        }
    }

    // Taps the file's thumbnail until the picker hands it to the app. Right after the folder
    // appears, the picker can swallow the first tap while thumbnails load.
    func open(_ prefix: String) throws {
        for _ in 0..<3 {
            pause(1.5)
            let cell = app.cells.matching(NSPredicate(format: "label BEGINSWITH[c] %@", prefix)).firstMatch
            let target = cell.exists ? cell : file(prefix)
            if target.exists { target.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.25)).tap() }
            if item("Statement details").waitForExistence(timeout: 12) { return }
        }
        print(app.debugDescription)
        throw NSError(domain: "Picker did not open " + prefix, code: 3)
    }

    var homeVisible: Bool { app.buttons["Previous month"].waitForExistence(timeout: 20) }

    func monthColumn(_ month: String) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@ AND label ENDSWITH %@", month, "spent")).firstMatch
    }

    // MARK: the flow

    func testFlow() throws {
        continueAfterFailure = false
        capture("start-01-first-launch")
        app.launch()
        XCTAssertTrue(item("Import statement PDF").waitForExistence(timeout: 15))
        capture("01-empty-ledger")
        capture("stop")

        capture("start-02-sync-sheet")
        try tap("Sync")
        capture("02-sync-top")
        drag(0.77, 0.27)
        capture("03-sync-bottom")
        try tap("Done")
        capture("stop")

        try clip03PickStatement()
    }

    func clip03PickStatement() throws {
        capture("start-03-pick-pdf-a")
        try tap("Import statement PDF")
        try pick("sample-statement")
        capture("04-picker-local")
        try open("sample-statement")
        capture("05-iso-details-empty")
        capture("stop")
        try clip04Details()
    }

    func clip04Details() throws {
        capture("start-04-details-iso")
        type("Assets:Checking", into: "Assets:Checking", still: "06-iso-details-typing")
        type("USD", into: "USD")
        capture("07-iso-details-filled")
        review(26)
        capture("stop")
        try clip05Review()
    }

    func clip05Review() throws {
        capture("start-05-review")
        capture("08-review-top")
        let finalDate = item("Nov 20, 2026")
        for index in 0..<22 {
            drag(0.86, 0.17)
            pause(0.4)
            if index == 5 { capture("09-review-middle") }
            if finalDate.isHittable { break }
        }
        drag(0.81, 0.40)
        capture("10-review-bottom")
        let coffee = app.switches.matching(identifier: "Include Coffee Corner")
        try XCTUnwrap(coffee.allElementsBoundByIndex.last(where: \.isHittable)).tap()
        capture("11-review-row-excluded")
        XCTAssertTrue(app.buttons["Add 25 transactions"].exists)
        try XCTUnwrap(coffee.allElementsBoundByIndex.first { $0.isHittable && ($0.value as? String) == "0" }).tap()
        XCTAssertTrue(app.buttons["Add 26 transactions"].waitForExistence(timeout: 5))
        try tap("Add 26 transactions")
        XCTAssertTrue(homeVisible)
        capture("12-home-november")
        capture("stop")
        try clip06Chart()
    }

    func clip06Chart() throws {
        capture("start-06-home-chart")
        drag(0.82, 0.32)
        capture("13-november-list")
        for _ in 0..<2 { drag(0.22, 0.86) }
        let august = monthColumn("August")
        XCTAssertTrue(august.isHittable)
        august.tap()
        capture("14-home-august")
        try tap("Previous month")
        capture("15-home-july")
        try tap("Next month")
        capture("16-home-august-return")
        drag(0.81, 0.32)
        capture("17-august-list")
        for _ in 0..<2 { drag(0.22, 0.86) }
        capture("stop")
        try clip07Ambiguous()
    }

    // Fixture B is seeded as z-ambiguous-dates.pdf so it sorts below the statement;
    // taps on the first row of the picker list can land on the search field.
    func clip07Ambiguous() throws {
        capture("start-07-pick-pdf-b-ambiguous")
        try tap("Import statement PDF")
        try pick("z-ambiguous-dates")
        capture("18-picker-ambiguous")
        try open("z-ambiguous-dates")
        capture("19-slash-details-empty")
        try tap("DD/MM/YYYY")
        type("Liabilities:Card", into: "Assets:Checking", still: "20-slash-details-typing")
        type("USD", into: "USD")
        capture("21-slash-details-currency")
        review(3)
        capture("22-slash-review")
        try tap("Add 3 transactions")
        XCTAssertTrue(homeVisible)
        capture("23-home-after-second-import")
        capture("stop")
        try clip08Dark()
    }

    func clip08Dark() throws {
        capture("start-08-dark-mode")
        capture("dark")
        capture("24-dark-home")
        try tap("Previous month")
        capture("25-dark-previous-month")
        try tap("Previous month")
        capture("26-dark-two-months-back")
        capture("stop")
    }

    // Run before testFlow on a freshly erased simulator, without recording. The first
    // document-picker sessions after an erase list files but don't hand them to the app.
    // Opening each fixture once (relaunching until it works) warms the Files provider.
    // Nothing is imported, so the ledger stays empty.
    func testWarmUp() throws {
        for fixture in ["sample-statement", "z-ambiguous-dates"] {
            var opened = false
            for _ in 0..<6 where !opened {
                app.launch()
                XCTAssertTrue(item("Import statement PDF").waitForExistence(timeout: 20))
                try tap("Import statement PDF")
                try pick(fixture)
                opened = (try? open(fixture)) != nil
                app.terminate()
                if !opened { pause(5) }
            }
            XCTAssertTrue(opened, "The document picker never opened \(fixture)")
        }
    }

    // Resume points for a run that failed partway (the ledger keeps earlier imports).
    func testResumeChart() throws { continueAfterFailure = false; app.launch(); XCTAssertTrue(homeVisible); try clip06Chart() }
    func testResumeAmbiguous() throws { continueAfterFailure = false; app.launch(); XCTAssertTrue(homeVisible); try clip07Ambiguous() }
    func testResumeDark() throws { continueAfterFailure = false; app.launch(); XCTAssertTrue(homeVisible); try clip08Dark() }
}
