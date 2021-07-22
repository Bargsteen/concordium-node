#!/usr/bin/env bash
set -euo pipefail


# Parameters
readonly version=${1:?"Please provide a version number (e.g. '1.0.2')"}
readonly developerIdApplication="Developer ID Application: Concordium Software Aps (K762RM4LQ3)"
readonly developerIdInstaller="Developer ID Installer: Concordium Software Aps (K762RM4LQ3)"

readonly GREEN='\033[0;32m'
readonly NC='\033[0m' # No Color

logInfo () {
    printf "\n${GREEN}$@${NC}\n"
}

readonly ghcVariant="x86_64-osx-ghc-8.10.4"

macPackageDir=$(pwd)
readonly macPackageDir
readonly nodeDir="$macPackageDir/../../../concordium-node"
readonly consensusDir="$macPackageDir/../../../concordium-consensus"
readonly toolsDir="$macPackageDir/tools"
readonly macdylibbundlerDir="$toolsDir/macdylibbundler-1.0.0"
readonly installDir="/Library/concordium-node/$version"
readonly templateDir="$macPackageDir/template"
readonly buildDir="$macPackageDir/build"
readonly distDir="$buildDir/dist" # TODO: rename
readonly packagesDir="$buildDir/packages"
readonly pkgFile="$packagesDir/concordium-node.pkg"
readonly signedPkgFile="$packagesDir/concordium-node-signed.pkg"


function clean() {
    if [ -d "$buildDir" ]; then
        logInfo "Cleaning '$buildDir' folder"
        rm -r "$buildDir"
    fi
}

function createBuildDirFromTemplate() {
    logInfo "Creating build folder from template..."
    cp -r "$templateDir" "$buildDir"
    sed -i '' -e 's/__VERSION__/'"$version"'/g' "$buildDir/distribution.xml"
    sed -i '' -e 's/__VERSION__/'"$version"'/g' "$buildDir/scripts/postinstall"
    logInfo "Done"
}

function compileConsensus() {
    cd "$consensusDir"
    logInfo "Building Consensus..."
    stack build
    logInfo "Done"
}

function compileNodeAndCollector() {
    cd "$nodeDir"
    logInfo "Building Node and Collector..."
    cargo build --bin concordium-node --bin node-collector --features collector --release
    logInfo "Done"
}

function compile() {
    compileConsensus
    compileNodeAndCollector
}

function copyBinaries() {
    logInfo "Copy concordium-node and node-collector binaries to '$distDir'.."
    mkdir "$distDir"
    cp "$nodeDir/target/release/concordium-node" "$distDir"
    cp "$nodeDir/target/release/node-collector" "$distDir"
    logInfo "Done"
}

function downloadGenesis() {
    logInfo "Downloading genesis.dat"
    curl -sSL "https://distribution.mainnet.concordium.software/data/genesis.dat" > "$distDir/genesis.dat"
    logInfo "Done"
}


function getDylibbundler() {
    logInfo "Getting dylibbundler..."

    if test -f "$macdylibbundlerDir/dylibbundler"
    then
        logInfo "Skipped: already exists"
    else
        logInfo " -- Downloading..."
        mkdir "$toolsDir"
        cd "$macPackageDir"
        curl -sSL "https://github.com/auriamg/macdylibbundler/archive/refs/tags/1.0.0.zip" > "$toolsDir/dylibbundler.zip" \
                    && logInfo " -- Unzipping..." \
                    && cd "$toolsDir" \
                    && unzip "dylibbundler.zip" \
                    && logInfo " -- Building..." \
                    && cd "$macdylibbundlerDir" \
                    && make
        logInfo "Done"
    fi
}

function collectDylibsFor() {
    local fileToFix=${1:?"Missing file to fix with dylibbundler"};
    cd "$distDir"
    "$macdylibbundlerDir/dylibbundler" --fix-file "$fileToFix" --bundle-deps --dest-dir "./libs" --install-path "@executable_path/libs/" --overwrite-dir \
        -s "$concordiumDylibDir" \
        -s "$stackSnapshotDir" \
        $stackLibDirs # Unquoted on purpose to use as arguments correctly
}

function collectDylibs() {
    logInfo "Collecting dylibs with dylibbundler..."

    concordiumDylibDir=$(stack --stack-yaml "$consensusDir/stack.yaml" path --local-install-root)"/lib/$ghcVariant"
    stackSnapshotDir=$(stack --stack-yaml "$consensusDir/stack.yaml" path --snapshot-install-root)"/lib/$ghcVariant"
    stackLibDirs=$(find "$(stack --stack-yaml "$consensusDir/stack.yaml" ghc -- --print-libdir)" -maxdepth 1 -type d | awk '{print "-s "$0}')
    readonly concordiumDylibDir
    readonly stackSnapshotDir
    readonly stackLibDirs

    logInfo " -- Processing concordium-node"
    collectDylibsFor "$distDir/concordium-node"
    logInfo " -- Processing node-collector"
    collectDylibsFor "$distDir/node-collector"

    logInfo "Done"
}

function signBinaries() {
    logInfo "Signing binaries..."
    # perm +111 finds the executable files
    find "$distDir" \
        -type f \
        -execdir sudo codesign -f --options runtime -s "$developerIdApplication" {} \;
    logInfo "Done"
}

function ensureDirExists() {
    local theDir=${1:?"ensureDirExists requires 1 parameter: directory"}
    if [ ! -d "$theDir" ]; then
        mkdir "$theDir"
    fi
}

function buildPackage() {
    logInfo "Building package..."
    ensureDirExists "$packagesDir"
    pkgbuild --identifier software.concordium.node \
        --version "$version" \
        --install-location "$installDir" \
        --root "$distDir" \
        "$pkgFile"
    logInfo "Done"
}

function buildProduct() {
    logInfo "Building product..."
    ensureDirExists "$packagesDir"
    productbuild \
        --distribution "$buildDir/distribution.xml" \
        --scripts "$buildDir/scripts" \
        --package-path "$pkgFile" \
        --resources "$buildDir/resources" \
        --sign "$developerIdInstaller" \
        "$signedPkgFile"
    logInfo "Done"
}

function notarize() {
    logInfo "Notarizing..."
    # FIXME: The keychain-profile part will not work on other computers
    xcrun notarytool submit \
        "$signedPkgFile" \
        --keychain-profile "notarytool" \
        --wait
    logInfo "Done"
}

function staple() {
    logInfo "Stapling..."
    xcrun stapler staple "$signedPkgFile"
    logInfo "Done"
}

function main() {
    clean
    createBuildDirFromTemplate
    compile
    copyBinaries
    downloadGenesis
    getDylibbundler
    collectDylibs
    signBinaries
    buildPackage
    buildProduct
    notarize
    staple
}

main
