//! What each role may do. One function, so the rule is read in one place.

use dionaea_meta::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Read reports of a site.
    Read,
    /// Change sites, keys, schemas, retention and erasure.
    Manage,
    /// Change who belongs to the organization and with which role.
    ManageMembers,
}

impl Permission {
    /// The least role that has the permission.
    pub fn least_role(self) -> Role {
        match self {
            Self::Read => Role::Viewer,
            Self::Manage => Role::Admin,
            Self::ManageMembers => Role::Owner,
        }
    }
}

pub fn allows(role: Role, permission: Permission) -> bool {
    role >= permission.least_role()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_role_has_exactly_the_permissions_it_should() {
        let table = [
            (Role::Viewer, [true, false, false]),
            (Role::Admin, [true, true, false]),
            (Role::Owner, [true, true, true]),
        ];
        for (role, expected) in table {
            let got = [
                allows(role, Permission::Read),
                allows(role, Permission::Manage),
                allows(role, Permission::ManageMembers),
            ];
            assert_eq!(got, expected, "{role:?}");
        }
    }
}
