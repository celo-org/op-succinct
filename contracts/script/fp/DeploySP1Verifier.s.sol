// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";

// Contracts
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";
import {SP1Verifier as SP1VerifierGroth16} from "@sp1-deployment/src/v5.0.0/SP1VerifierGroth16.sol";
import {SP1Verifier as SP1VerifierPlonk} from "@sp1-deployment/src/v5.0.0/SP1VerifierPlonk.sol";

contract DeploySP1Verifier is Script {
    function run() public {
        address sp1Verifier;
        string memory verifierType = vm.envString("SP1_VERIFIER_TYPE");
        
        vm.startBroadcast();
        if (equal(verifierType, "mock")) {
            sp1Verifier = address(new SP1MockVerifier());
        } else if (equal(verifierType, "groth16")) {
            sp1Verifier = address(new SP1VerifierGroth16());
        } else if (equal(verifierType, "plonk")) {
            sp1Verifier = address(new SP1VerifierPlonk());
        } else {
            revert("Unsupported SP1 verifier type!");
        }
        vm.stopBroadcast();

        console.log("Deployed SP1 Verifier at:", sp1Verifier);
    }

    function equal(string memory a, string memory b) internal pure returns (bool) {
        return bytes(a).length == bytes(b).length && keccak256(bytes(a)) == keccak256(bytes(b));
    }
}
