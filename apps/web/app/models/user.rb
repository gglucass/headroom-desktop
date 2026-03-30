class User < ApplicationRecord
  has_secure_password

  enum :plan, { free: "free", paid: "paid" }, validate: true

  before_validation :normalize_email

  validates :email, presence: true, uniqueness: true
  validates :password, length: { minimum: 8 }, if: -> { new_record? || password.present? }

  private

  def normalize_email
    self.email = email.to_s.strip.downcase
  end
end
