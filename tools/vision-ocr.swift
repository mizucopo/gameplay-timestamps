import Foundation
import Vision

struct InputError: Error, CustomStringConvertible {
    let description: String
    init(_ message: String) { description = message }
}

func makeRequest() throws -> VNRecognizeTextRequest {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = false
    guard try request.supportedRecognitionLanguages().contains("ja-JP") else {
        throw InputError("この環境では日本語の画面文字を認識できません")
    }
    request.recognitionLanguages = ["ja-JP", "en-US"]
    return request
}

func recognize(_ jpeg: Data, using request: VNRecognizeTextRequest) throws -> String {
    try VNImageRequestHandler(data: jpeg).perform([request])
    return (request.results ?? []).compactMap { observation in
        guard let candidate = observation.topCandidates(1).first,
              candidate.confidence >= 0.5,
              candidate.string.rangeOfCharacter(from: .alphanumerics) != nil else { return nil }
        return candidate.string
    }.joined(separator: "\n")
}

// Pipe reads can end before the complete JPEG has arrived.
func readExactly(_ count: Int) throws -> Data {
    var result = Data()
    while result.count < count {
        guard let data = try FileHandle.standardInput.read(upToCount: count - result.count),
              !data.isEmpty else { break }
        result.append(data)
    }
    return result
}

func runBatch(_ request: VNRecognizeTextRequest) throws {
    let encoder = JSONEncoder()
    while true {
        let header = try readExactly(4)
        if header.isEmpty { return }
        guard header.count == 4 else { throw InputError("JPEG 長の途中で入力が終了しました") }
        let length = header.reduce(0) { $0 * 256 + Int($1) }
        guard (1...64 * 1024 * 1024).contains(length) else { throw InputError("JPEG 長が不正です") }
        try autoreleasepool {
            let jpeg = try readExactly(length)
            guard jpeg.count == length else { throw InputError("JPEG の途中で入力が終了しました") }
            let text = try recognize(jpeg, using: request)
            var response = try encoder.encode(text)
            response.append(0x0a)
            try FileHandle.standardOutput.write(contentsOf: response)
        }
    }
}

do {
    let request = try makeRequest()
    switch Array(CommandLine.arguments.dropFirst()) {
    case []:
        let jpeg = try FileHandle.standardInput.readToEnd() ?? Data()
        print(try recognize(jpeg, using: request))
    case ["--batch"]:
        try runBatch(request)
    default:
        throw InputError("使い方: vision-ocr [--batch]")
    }
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(1)
}
