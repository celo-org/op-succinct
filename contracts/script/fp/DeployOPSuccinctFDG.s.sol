// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {Claim, GameType, Hash, OutputRoot, Duration} from "src/dispute/lib/Types.sol";
import {LibString} from "@solady/utils/LibString.sol";

// Interfaces
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {ISP1Verifier} from "@sp1-contracts/src/ISP1Verifier.sol";
import {IAnchorStateRegistry} from "interfaces/dispute/IAnchorStateRegistry.sol";
import {ISuperchainConfig} from "interfaces/L1/ISuperchainConfig.sol";
import {IOptimismPortal2} from "interfaces/L1/IOptimismPortal2.sol";

// Contracts
import {AccessManager} from "../../src/fp/AccessManager.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OPSuccinctFaultDisputeGame} from "../../src/fp/OPSuccinctFaultDisputeGame.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";
import {SP1Verifier} from "@sp1-contracts/src/v5.0.0/SP1VerifierGroth16.sol";

// Utils

struct OPContractAddresses {
    address anchorStateRegistryAddress;
    address disputeGameFactoryAddress;
    address portalAddress;
}

contract DeployOPSuccinctFDG is Script {
    function getAddrFromEnv(string memory _envKey) internal view returns (address addr_) {
        if (vm.envOr(_envKey, address(0)) != address(0)) {
            addr_ = vm.envAddress(_envKey);
        } else {
            addr_ = address(0);
        }
    }

    function getOrDeployOPContracts() internal returns (OPContractAddresses memory) {
        address factoryAddr = getAddrFromEnv("FACTORY_ADDRESS");
        address disputeGameFactoryAddress = factoryAddr;

        address registryProxyAddr = getAddrFromEnv("ANCHOR_STATE_REGISTRY_ADDRESS");

        // Use provided OptimismPortal2 address if given, otherwise deploy MockOptimismPortal2.
        address portalAddress = getAddrFromEnv("OPTIMISM_PORTAL2_ADDRESS");
        address anchorStateRegistryAddress = registryProxyAddr;
        console.log("Using AnchorStateRegistry:", anchorStateRegistryAddress);
        console.log("Using DisputeGameFactory:", disputeGameFactoryAddress);
        console.log("Using Portal:", portalAddress);
        return OPContractAddresses({
            disputeGameFactoryAddress: disputeGameFactoryAddress,
            anchorStateRegistryAddress: anchorStateRegistryAddress,
            portalAddress: portalAddress
        });
    }

    function run() public {
        vm.startBroadcast();

        OPContractAddresses memory contracts = getOrDeployOPContracts();

        // Deploy the access manager contract.
        AccessManager accessManager = new AccessManager();
        console.log("Access manager:", address(accessManager));

        // Configure access control based on `PERMISSIONLESS_MODE` flag.
        if (vm.envOr("PERMISSIONLESS_MODE", false)) {
            // Set to permissionless games (anyone can propose and challenge).
            accessManager.setProposer(address(0), true);
            accessManager.setChallenger(address(0), true);
            console.log("Access Manager configured for permissionless mode");
        } else {
            // Set proposers from comma-separated list.
            string memory proposersStr = vm.envOr("PROPOSER_ADDRESSES", string(""));
            if (bytes(proposersStr).length > 0) {
                string[] memory proposers = LibString.split(proposersStr, ",");
                for (uint256 i = 0; i < proposers.length; i++) {
                    address proposer = vm.parseAddress(proposers[i]);
                    if (proposer != address(0)) {
                        accessManager.setProposer(proposer, true);
                        console.log("Added proposer:", proposer);
                    }
                }
            }

            // Set challengers from comma-separated list.
            string memory challengersStr = vm.envOr("CHALLENGER_ADDRESSES", string(""));
            if (bytes(challengersStr).length > 0) {
                string[] memory challengers = LibString.split(challengersStr, ",");
                for (uint256 i = 0; i < challengers.length; i++) {
                    address challenger = vm.parseAddress(challengers[i]);
                    if (challenger != address(0)) {
                        accessManager.setChallenger(challenger, true);
                        console.log("Added challenger:", challenger);
                    }
                }
            }
        }

        IDisputeGameFactory factory = IDisputeGameFactory(contracts.disputeGameFactoryAddress);

        // Config values dependent on the `USE_SP1_MOCK_VERIFIER` flag.
        address sp1VerifierAddress;
        bytes32 rollupConfigHash;
        bytes32 aggregationVkey;
        bytes32 rangeVkeyCommitment;

        // Get or deploy SP1 verifier based on environment variable.
        if (vm.envOr("USE_SP1_MOCK_VERIFIER", false)) {
            // Deploy mock verifier for testing.
            SP1MockVerifier sp1Verifier = new SP1MockVerifier();
            sp1VerifierAddress = address(sp1Verifier);
            console.log("Using SP1 Mock Verifier:", address(sp1Verifier));

            rollupConfigHash = bytes32(0);
            aggregationVkey = bytes32(0);
            rangeVkeyCommitment = bytes32(0);
        } else {
            if (vm.envOr("VERIFIER_ADDRESS", false)) {
                // Use provided verifier address for production.
                sp1VerifierAddress = vm.envAddress("VERIFIER_ADDRESS");
            } else {
                // Deploy contract Groth16 directly without the gateway,
                // since this requires working around library inconsistencies and
                // additional setup on non-standard networks
                sp1VerifierAddress = address(new SP1Verifier());
            }
            console.log("Using SP1 Verifier:", sp1VerifierAddress);

            rollupConfigHash = vm.envBytes32("ROLLUP_CONFIG_HASH");
            aggregationVkey = vm.envBytes32("AGGREGATION_VKEY");
            rangeVkeyCommitment = vm.envBytes32("RANGE_VKEY_COMMITMENT");
        }

        // This instantiates the implementation contract
        // that later will get cloned and initialized for each dispute-game
        // create() on the factory
        OPSuccinctFaultDisputeGame gameImpl = new OPSuccinctFaultDisputeGame(
            Duration.wrap(uint64(vm.envUint("MAX_CHALLENGE_DURATION"))),
            Duration.wrap(uint64(vm.envUint("MAX_PROVE_DURATION"))),
            IDisputeGameFactory(contracts.disputeGameFactoryAddress),
            ISP1Verifier(sp1VerifierAddress),
            rollupConfigHash,
            aggregationVkey,
            rangeVkeyCommitment,
            vm.envOr("CHALLENGER_BOND_WEI", uint256(0.001 ether)),
            IAnchorStateRegistry(contracts.anchorStateRegistryAddress),
            accessManager
        );

        GameType gameType = GameType.wrap(uint32(vm.envUint("GAME_TYPE")));

        // Set initial bond and implementation in factory.
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));
        factory.setInitBond(gameType, vm.envOr("INITIAL_BOND_WEI", uint256(0.001 ether)));
        IOptimismPortal2 portal = IOptimismPortal2(payable(contracts.portalAddress));
        portal.setRespectedGameType(gameType);

        vm.stopBroadcast();

        // Log deployed addresses.
        // console.log("Factory Proxy:", address(factoryProxy));
        console.log("Game Implementation:", address(gameImpl));
        console.log("SP1 Verifier:", sp1VerifierAddress);
    }
}
