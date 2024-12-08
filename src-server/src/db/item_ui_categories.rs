//! `SeaORM` Entity, @generated by sea-orm-codegen 1.1.2

use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize)]
#[sea_orm(table_name = "ItemUICategories")]
pub struct Model {
    #[sea_orm(column_name = "Id", primary_key)]
    pub id: u32,
    #[sea_orm(column_name = "Name", column_type = "custom(\"LONGTEXT\")")]
    pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::items::Entity")]
    Items,
}

impl Related<super::items::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Items.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
