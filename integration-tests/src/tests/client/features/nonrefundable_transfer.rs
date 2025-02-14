//! Non-refundable transfers during account creation allow to sponsor an
//! accounts storage staking balance without that someone being able to run off
//! with the money.
//!
//! This feature introduces the NonrefundableStorageTransfer action.
//!
//! NEP: https://github.com/near/NEPs/pull/491

use near_chain_configs::Genesis;
use near_chain_configs::NEAR_BASE;
use near_client::test_utils::TestEnv;
use near_crypto::{InMemorySigner, KeyType, PublicKey};
use near_primitives::errors::{
    ActionError, ActionErrorKind, ActionsValidationError, InvalidTxError, TxExecutionError,
};
use near_primitives::transaction::{
    Action, AddKeyAction, CreateAccountAction, DeleteAccountAction, DeployContractAction,
    NonrefundableStorageTransferAction, SignedTransaction, TransferAction,
};
use near_primitives::types::{AccountId, Balance};
use near_primitives::utils::{derive_eth_implicit_account_id, derive_near_implicit_account_id};
use near_primitives::version::{ProtocolFeature, ProtocolVersion};
use near_primitives::views::{
    ExecutionStatusView, FinalExecutionOutcomeView, QueryRequest, QueryResponseKind,
};
use near_primitives_core::account::{AccessKey, AccessKeyPermission};
use nearcore::test_utils::TestEnvNightshadeSetupExt;
use testlib::fees_utils::FeeHelper;

use crate::node::RuntimeNode;

#[derive(Clone, Debug)]
struct Transfers {
    /// Regular transfer amount (if any).
    regular_amount: Balance,
    /// Non-refundable transfer amount (if any).
    nonrefundable_amount: Balance,
    /// Whether non-refundable transfer action should be first in the receipt.
    nonrefundable_transfer_first: bool,
}

/// Different `Transfers` configurations, where we only test cases where non-refundable transfer happens.
const TEST_CASES: [Transfers; 3] = [
    Transfers { regular_amount: 0, nonrefundable_amount: 1, nonrefundable_transfer_first: true },
    Transfers { regular_amount: 1, nonrefundable_amount: 1, nonrefundable_transfer_first: true },
    Transfers { regular_amount: 1, nonrefundable_amount: 1, nonrefundable_transfer_first: false },
];

struct TransferConfig {
    /// Describes transfers configuration we are interested in.
    transfers: Transfers,
    /// True if the receipt should create account.
    account_creation: bool,
    /// Differentaties between named and implicit account creation, if `account_creation` is true.
    implicit_account_creation: bool,
    /// Whether the last action in the receipt should deploy a contract.
    deploy_contract: bool,
}

/// Default sender to use in tests of this module.
fn sender() -> AccountId {
    "test0".parse().unwrap()
}

/// Default receiver to use in tests of this module.
fn receiver() -> AccountId {
    "test1".parse().unwrap()
}

/// Default signer (corresponding to the default sender) to use in tests of this module.
fn signer() -> InMemorySigner {
    InMemorySigner::from_seed(sender(), KeyType::ED25519, "test0")
}

/// Creates a test environment using given protocol version (if some).
fn setup_env_with_protocol_version(protocol_version: Option<ProtocolVersion>) -> TestEnv {
    let mut genesis = Genesis::test(vec![sender(), receiver()], 1);
    if let Some(protocol_version) = protocol_version {
        genesis.config.protocol_version = protocol_version;
    }
    TestEnv::builder(&genesis.config).nightshade_runtimes(&genesis).build()
}

/// Creates a test environment using default protocol version.
fn setup_env() -> TestEnv {
    setup_env_with_protocol_version(None)
}

fn fee_helper() -> FeeHelper {
    let node = RuntimeNode::new(&sender());
    crate::tests::standard_cases::fee_helper(&node)
}

fn get_nonce(env: &mut TestEnv, signer: &InMemorySigner) -> u64 {
    let request = QueryRequest::ViewAccessKey {
        account_id: signer.account_id.clone(),
        public_key: signer.public_key.clone(),
    };
    match env.query_view(request).unwrap().kind {
        QueryResponseKind::AccessKey(view) => view.nonce,
        _ => panic!("wrong query response"),
    }
}

fn account_exists(env: &mut TestEnv, account_id: AccountId) -> bool {
    let request = QueryRequest::ViewAccount { account_id };
    env.query_view(request).is_ok()
}

fn execute_transaction_from_actions(
    env: &mut TestEnv,
    actions: Vec<Action>,
    signer: &InMemorySigner,
    receiver: AccountId,
) -> Result<FinalExecutionOutcomeView, InvalidTxError> {
    let tip = env.clients[0].chain.head().unwrap();
    let nonce = get_nonce(env, signer);
    let tx = SignedTransaction::from_actions(
        nonce + 1,
        signer.account_id.clone(),
        receiver,
        signer,
        actions,
        tip.last_block_hash,
    );
    let tx_result = env.execute_tx(tx);
    let height = env.clients[0].chain.head().unwrap().height;
    for i in 0..2 {
        env.produce_block(0, height + 1 + i);
    }
    tx_result
}

/// Submits a transfer (regular, non-refundable, or both).
/// Can possibly create an account or deploy a contract, depending on the `config`.
///
/// This methods checks that the balance is subtracted from the sender and added
/// to the receiver, if the status was ok. No checks are done on an error.
fn exec_transfers(
    env: &mut TestEnv,
    signer: InMemorySigner,
    receiver: AccountId,
    config: TransferConfig,
) -> Result<FinalExecutionOutcomeView, InvalidTxError> {
    let sender_pre_balance = env.query_balance(sender());
    let (receiver_before_amount, receiver_before_nonrefundable) = if config.account_creation {
        (0, 0)
    } else {
        let receiver_before = env.query_account(receiver.clone());
        (receiver_before.amount, receiver_before.nonrefundable)
    };

    let mut actions = vec![];

    if config.account_creation && !config.implicit_account_creation {
        actions.push(Action::CreateAccount(CreateAccountAction {}));
        actions.push(Action::AddKey(Box::new(AddKeyAction {
            public_key: PublicKey::from_seed(KeyType::ED25519, receiver.as_str()),
            access_key: AccessKey { nonce: 0, permission: AccessKeyPermission::FullAccess },
        })));
    }

    if config.transfers.nonrefundable_transfer_first && config.transfers.nonrefundable_amount > 0 {
        actions.push(Action::NonrefundableStorageTransfer(NonrefundableStorageTransferAction {
            deposit: config.transfers.nonrefundable_amount,
        }));
    }
    if config.transfers.regular_amount > 0 {
        actions.push(Action::Transfer(TransferAction { deposit: config.transfers.regular_amount }));
    }
    if !config.transfers.nonrefundable_transfer_first && config.transfers.nonrefundable_amount > 0 {
        actions.push(Action::NonrefundableStorageTransfer(NonrefundableStorageTransferAction {
            deposit: config.transfers.nonrefundable_amount,
        }));
    }

    if config.deploy_contract {
        let contract = near_test_contracts::sized_contract(1500 as usize);
        actions.push(Action::DeployContract(DeployContractAction { code: contract.to_vec() }))
    }

    let tx_result = execute_transaction_from_actions(env, actions, &signer, receiver.clone());

    let outcome = match &tx_result {
        Ok(outcome) => outcome,
        _ => {
            return tx_result;
        }
    };

    if !matches!(outcome.status, near_primitives::views::FinalExecutionStatus::SuccessValue(_)) {
        return tx_result;
    }

    let gas_cost = outcome.tokens_burnt();
    assert_eq!(
        sender_pre_balance
            - config.transfers.regular_amount
            - config.transfers.nonrefundable_amount
            - gas_cost,
        env.query_balance(sender())
    );

    let receiver_expected_amount_after = receiver_before_amount + config.transfers.regular_amount;
    let receiver_expected_non_refundable_after =
        receiver_before_nonrefundable + config.transfers.nonrefundable_amount;
    let receiver_after = env.query_account(receiver);
    assert_eq!(receiver_after.amount, receiver_expected_amount_after);
    assert_eq!(receiver_after.nonrefundable, receiver_expected_non_refundable_after);

    tx_result
}

fn delete_account(
    env: &mut TestEnv,
    signer: &InMemorySigner,
    beneficiary_id: AccountId,
) -> Result<FinalExecutionOutcomeView, InvalidTxError> {
    let actions = vec![Action::DeleteAccount(DeleteAccountAction { beneficiary_id })];
    execute_transaction_from_actions(env, actions, &signer, signer.account_id.clone())
}

/// Can delete account with non-refundable storage.
#[test]
fn deleting_account_with_non_refundable_storage() {
    let mut env = setup_env();
    let new_account_id: AccountId = "subaccount.test0".parse().unwrap();
    let new_account = InMemorySigner::from_seed(
        new_account_id.clone(),
        KeyType::ED25519,
        new_account_id.as_str(),
    );
    let regular_amount = 10u128.pow(20);
    let nonrefundable_amount = NEAR_BASE;
    // Create account with non-refundable storage.
    // Send some NEAR (refundable) so that the new account is able to pay the gas for its deletion in the next transaction.
    // Deploy a contract that does not fit within Zero-balance account limit.
    let create_account_tx_result = exec_transfers(
        &mut env,
        signer(),
        new_account_id.clone(),
        TransferConfig {
            transfers: Transfers {
                regular_amount,
                nonrefundable_amount,
                nonrefundable_transfer_first: true,
            },
            account_creation: true,
            implicit_account_creation: false,
            deploy_contract: true,
        },
    );
    create_account_tx_result.unwrap().assert_success();

    // Delete the new account (that has 1 NEAR of non-refundable balance).
    let beneficiary_id = receiver();
    let beneficiary_before = env.query_account(beneficiary_id.clone());
    let delete_account_tx_result = delete_account(&mut env, &new_account, beneficiary_id.clone());
    delete_account_tx_result.unwrap().assert_success();
    assert!(!account_exists(&mut env, new_account_id));

    // Check that the beneficiary account received the remaining balance from the deleted account,
    // but none of the non-refundable balance.
    let beneficiary_after = env.query_account(beneficiary_id);
    assert_eq!(
        beneficiary_after.amount,
        beneficiary_before.amount + regular_amount - fee_helper().prepaid_delete_account_cost()
    );
    assert_eq!(beneficiary_after.nonrefundable, beneficiary_before.nonrefundable);
}

/// Non-refundable balance cannot be transferred.
#[test]
fn non_refundable_balance_cannot_be_transferred() {
    let mut env = setup_env();
    let new_account_id: AccountId = "subaccount.test0".parse().unwrap();
    let new_account = InMemorySigner::from_seed(
        new_account_id.clone(),
        KeyType::ED25519,
        new_account_id.as_str(),
    );
    // The `new_account` is created with 1 NEAR non-refundable balance.
    let create_account_tx_result = exec_transfers(
        &mut env,
        signer(),
        new_account_id.clone(),
        TransferConfig {
            transfers: Transfers {
                regular_amount: 0,
                nonrefundable_amount: NEAR_BASE,
                nonrefundable_transfer_first: true,
            },
            account_creation: true,
            implicit_account_creation: false,
            deploy_contract: false,
        },
    );
    create_account_tx_result.unwrap().assert_success();

    // Although `new_account` has 1 NEAR non-refundable balance, it cannot make neither refundable nor non-refundable transfer of 1 yoctoNEAR.
    for nonrefundable in [false, true] {
        let transfer_tx_result = exec_transfers(
            &mut env,
            new_account.clone(),
            receiver(),
            TransferConfig {
                transfers: Transfers {
                    regular_amount: if nonrefundable { 0 } else { 1 },
                    nonrefundable_amount: if nonrefundable { 1 } else { 0 },
                    nonrefundable_transfer_first: true,
                },
                account_creation: false,
                implicit_account_creation: false,
                deploy_contract: false,
            },
        );
        match transfer_tx_result {
            Err(InvalidTxError::NotEnoughBalance { signer_id, balance, .. }) => {
                assert_eq!(signer_id, new_account_id);
                assert_eq!(balance, 0);
            }
            _ => panic!("Expected NotEnoughBalance error"),
        }
    }
}

/// Non-refundable balance allows to have account with zero balance and more than 1kB of state.
#[test]
fn non_refundable_balance_allows_1kb_state_with_zero_balance() {
    let mut env = setup_env();
    let new_account_id: AccountId = "subaccount.test0".parse().unwrap();
    let tx_result = exec_transfers(
        &mut env,
        signer(),
        new_account_id,
        TransferConfig {
            transfers: Transfers {
                regular_amount: 0,
                nonrefundable_amount: NEAR_BASE / 5,
                nonrefundable_transfer_first: true,
            },
            account_creation: true,
            implicit_account_creation: false,
            deploy_contract: true,
        },
    );
    tx_result.unwrap().assert_success();
}

/// Non-refundable transfer successfully adds non-refundable balance when creating named account.
#[test]
fn non_refundable_transfer_create_named_account() {
    for (index, transfers) in TEST_CASES.iter().enumerate() {
        let account_name = format!("subaccount{}.test0", index).to_string();
        let new_account_id: AccountId = account_name.parse().unwrap();
        let tx_result = exec_transfers(
            &mut setup_env(),
            signer(),
            new_account_id,
            TransferConfig {
                transfers: transfers.clone(),
                account_creation: true,
                implicit_account_creation: false,
                deploy_contract: false,
            },
        );
        tx_result.unwrap().assert_success();
    }
}

/// Non-refundable transfer successfully adds non-refundable balance when creating NEAR-implicit account.
#[test]
fn non_refundable_transfer_create_near_implicit_account() {
    for (index, transfers) in TEST_CASES.iter().enumerate() {
        let public_key =
            PublicKey::from_seed(KeyType::ED25519, &format!("near{}", index).to_string());
        let new_account_id = derive_near_implicit_account_id(public_key.unwrap_as_ed25519());
        let tx_result = exec_transfers(
            &mut setup_env(),
            signer(),
            new_account_id.clone(),
            TransferConfig {
                transfers: transfers.clone(),
                account_creation: true,
                implicit_account_creation: true,
                deploy_contract: false,
            },
        );
        if transfers.regular_amount == 0 {
            tx_result.unwrap().assert_success();
        } else {
            // Non-refundable transfer must be the only action in an implicit account creation transaction.
            let status = &tx_result.unwrap().receipts_outcome[0].outcome.status;
            assert!(matches!(
                status,
                ExecutionStatusView::Failure(TxExecutionError::ActionError(
                    ActionError { kind: ActionErrorKind::AccountDoesNotExist { account_id }, .. }
                )) if *account_id == new_account_id,
            ));
        }
    }
}

/// Non-refundable transfer successfully adds non-refundable balance when creating ETH-implicit account.
#[test]
fn non_refundable_transfer_create_eth_implicit_account() {
    for (index, transfers) in TEST_CASES.iter().enumerate() {
        let public_key =
            PublicKey::from_seed(KeyType::SECP256K1, &format!("eth{}", index).to_string());
        let new_account_id = derive_eth_implicit_account_id(public_key.unwrap_as_secp256k1());
        let tx_result = exec_transfers(
            &mut setup_env(),
            signer(),
            new_account_id.clone(),
            TransferConfig {
                transfers: transfers.clone(),
                account_creation: true,
                implicit_account_creation: true,
                deploy_contract: false,
            },
        );
        if transfers.regular_amount == 0 {
            tx_result.unwrap().assert_success();
        } else {
            // Non-refundable transfer must be the only action in an implicit account creation transaction.
            let status = &tx_result.unwrap().receipts_outcome[0].outcome.status;
            assert!(matches!(
                status,
                ExecutionStatusView::Failure(TxExecutionError::ActionError(
                    ActionError { kind: ActionErrorKind::AccountDoesNotExist { account_id }, .. }
                )) if *account_id == new_account_id,
            ));
        }
    }
}

/// Non-refundable transfer is rejected on existing account.
#[test]
fn reject_non_refundable_transfer_existing_account() {
    for transfers in TEST_CASES {
        let tx_result = exec_transfers(
            &mut setup_env(),
            signer(),
            receiver(),
            TransferConfig {
                transfers,
                account_creation: false,
                implicit_account_creation: false,
                deploy_contract: false,
            },
        );
        let status = &tx_result.unwrap().receipts_outcome[0].outcome.status;
        assert!(matches!(
            status,
            ExecutionStatusView::Failure(TxExecutionError::ActionError(
                ActionError { kind: ActionErrorKind::NonRefundableBalanceToExistingAccount { account_id }, .. }
            )) if *account_id == receiver(),
        ));
    }
}

/// During the protocol upgrade phase, before the voting completes, we must not
/// include non-refundable transfer actions on the chain.
///
/// The correct way to handle it is to reject transaction before they even get
/// into the transaction pool. Hence, we check that an `InvalidTxError` error is
/// returned for older protocol versions.
#[test]
fn reject_non_refundable_transfer_in_older_versions() {
    let mut env = setup_env_with_protocol_version(Some(
        ProtocolFeature::NonRefundableBalance.protocol_version() - 1,
    ));
    for transfers in TEST_CASES {
        let tx_result = exec_transfers(
            &mut env,
            signer(),
            receiver(),
            TransferConfig {
                transfers,
                account_creation: false,
                implicit_account_creation: false,
                deploy_contract: false,
            },
        );
        assert_eq!(
            tx_result,
            Err(InvalidTxError::ActionsValidation(
                ActionsValidationError::UnsupportedProtocolFeature {
                    protocol_feature: "NonRefundableBalance".to_string(),
                    version: ProtocolFeature::NonRefundableBalance.protocol_version()
                }
            ))
        );
    }
}
