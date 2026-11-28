// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "nautilus-macos-probe",
    platforms: [.macOS(.v26)],
    dependencies: [
        .package(url: "https://github.com/apple/containerization.git", exact: "0.57.0"),
    ],
    targets: [
        .executableTarget(
            name: "nautilus-pod-probe",
            dependencies: [
                .product(name: "Containerization", package: "containerization"),
                .product(name: "ContainerizationOCI", package: "containerization"),
            ]
        ),
    ]
)
